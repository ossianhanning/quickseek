# NAS agent
Design for `quickseek-agent`: a small Linux program that indexes a NAS where the files are and
keeps the Windows service's copy current over the LAN, so NAS shares behave like any other
volume in search, the tree, folder sizes, growth history and the MCP tools.

Status: design accepted for the first version; the decisions below are what is built. Target
NAS for the first trial: a UGOS Pro box (Debian 12), kernel 6.18, 4 cores, 7.5 GB RAM, two
16 TB disks in mdadm RAID1 → LVM → btrfs at `/volume1`, shares `/volume1/data` and
`/volume1/media` (about 590,000 files), Docker 29.6, no password-less sudo.

## What it replaces, and when it wins
Today a share is a *folder index*: the service walks `\\nas\share` over SMB and keeps it current
with `ReadDirectoryChangesW` (SMB2 change notify) plus a periodic rescan. That keeps working and
stays the default. The agent is an option for people who can run a container on their NAS.

| | SMB folder index | Agent |
|---|---|---|
| Setup | nothing on the NAS | a container with read access to the volume, then pairing |
| First index of 590k files | a directory listing per folder over SMB: minutes, and it loads `smbd` | a local walk on the NAS, then ~6 MB to the PC |
| Service restart / PC wakes up | cached copy, then a full re-walk (or trust the cache until the rescan interval) | deltas since the last change it saw, or one compressed transfer; no re-walk |
| Credentials | the service needs the user's SMB credentials ("waiting for credentials") | a paired token; no SMB login for indexing |
| Change bursts (a 20k-file copy, an unpacked archive) | the 64 KB notify buffer overflows → full re-walk over SMB | the kernel queue is drained by the agent; a burst is a burst of small deltas |
| Changes made on the NAS itself (containers, downloads) | seen if Samba's kernel change notify reports them (it usually does) | seen (fanotify sees every writer) |
| Growth history while the PC is off | none for those days | the agent snapshots daily; the PC imports days it missed |
| Live update latency | ~immediate while the notify session is up | ~0.2-0.5 s (batching window + LAN) |
| Search speed | identical: both are a local in-memory index on the PC | identical |
| What a Windows user can see | what their SMB account may list | everything the agent can read (it reads the volume as root): names of files their account might not open |

So the agent wins on the first index, on restarts, on bursts, on credentials and on history; it
does not make searching faster (that already happens on the PC), and it costs a privileged
container and a wider view of the files. For a single-user home NAS like this one the trade is
clearly worth it; for a shared office NAS with per-user ACLs it may not be.

## Shape of the system
```
NAS                                             Windows PC
quickseek-agent (container)                     quickseek service (LocalSystem)
  walk /volume1/data, /volume1/media              remote volume \\mynas.local\data   ─┐
  fanotify (FS mark on /volume1)                  remote volume \\mynas.local\media  ─┤ Index, like
  in-memory tree, ids, change log                 cache: <db>\remote\*.idx            │ any volume:
  daily snapshots (/config/history)               history: imports missed days        │ search, tree,
  TLS server :48623 ───────────── TLS, pinned ──► one connection per share            │ growth, MCP
```

The agent is the source of truth for names and sizes; the PC keeps a full **replica** of each
share as a normal `Index`. Every search, sort, wildcard, folder-size tree and history comparison
then runs through the existing code with no remote round trip, and the replica on disk is the
offline cache.

## Change tracking
Options considered:

1. **fanotify, `FAN_MARK_FILESYSTEM` with `FAN_REPORT_DFID_NAME`** (needs `CAP_SYS_ADMIN`). One
   mark covers a whole filesystem; events carry the parent directory's file handle plus the name,
   and `FAN_RENAME` (5.17+) reports both ends of a rename in one event. No per-directory setup, no
   watch limit, sees every writer.
2. **inotify, one watch per directory**. Works unprivileged and on any filesystem, but needs a
   watch per directory (fine here: `max_user_watches` is 2,000,000, the shares have tens of
   thousands of folders), races on new directories (files created before the watch is added are
   found by listing the new directory right away), and renames arrive as a cookie-paired
   MOVED_FROM/MOVED_TO.
3. **btrfs generation scans** (`btrfs subvolume find-new`, i.e. `BTRFS_IOC_TREE_SEARCH` over
   inodes newer than a generation). Cheap to ask "what changed since generation G", and it
   survives the agent being stopped. But it needs `CAP_SYS_ADMIN` too, reports inodes rather than
   paths (each needs `BTRFS_IOC_INO_PATHS`), misses deletions (a deleted inode simply isn't there)
   and only exists on btrfs.

Measured with a small C probe on kernel 6.18 (WSL2, a loop-mounted btrfs image with one
subvolume; the same kernel series as the test NAS):
`FAN_MARK_FILESYSTEM` on the btrfs **root** mount works and reports events from inside
subvolumes; on a subvolume path or a bind mount of a subvolume it fails with `EXDEV`. On the test NAS
`/volume1` is the root subvolume (`subvolid=5`) while `/volume1/data` and `/volume1/media` are
subvolumes (inode 256, their own `st_dev`). So the container must see `/volume1` itself to use
fanotify; mounting only the two shares limits it to inotify.

**Decision.**

- **Primary: fanotify filesystem mark** on the mount that holds the shares (`/volume1`, mounted
  read-only into the container). Events for directories the agent doesn't track (Docker's
  overlay layers under `/volume1/@docker`, `@appstore`, ...) are dropped after one hash lookup of
  the directory handle. Mask: create, delete, rename, close-after-write, modify, attrib, with
  `FAN_ONDIR`. The queue is unlimited (`FAN_UNLIMITED_QUEUE`) and drained by a dedicated
  thread; `FAN_Q_OVERFLOW` (or our own backlog cap) triggers the gap-filler.
- **Fallback: inotify per directory**, chosen automatically when fanotify isn't allowed (no
  `CAP_SYS_ADMIN`, `EXDEV` because only subvolumes were mounted, another filesystem). Same
  reconcile path, so both are tested by the same tests.
- **Gap-filler on btrfs: a catch-up by generation** (below), after lost events, after a
  restart and every minute. **Elsewhere: a diffing re-walk**, after lost events and daily. Both
  diff against the tree, so ids of unchanged entries survive and only real differences go to
  the PC. Walks and full re-reads run on their own thread while live events keep being applied;
  the events seen meanwhile are applied again after the merge, since the listing may predate
  them (on the test NAS a 3-minute walk under load had held a live change up for 24 s before this).

Events are not applied blindly: each `(directory, name)` an event names is *reconciled* by
`lstat`ing it and comparing with the tree (exists → add or update, gone → remove, a new
directory → walk it). That makes ordering, duplicates and the "created before the watch" race
harmless, and batches of events for the same name collapse into one `lstat`. Events are
batched for 150 ms.

## Reading btrfs like the MFT
On btrfs (where the agent may search trees: `CAP_SYS_ADMIN`; `--index walk` turns it off) a
share isn't walked; its subvolume's tree is read directly, as Quickseek reads NTFS's `$MFT`
(`crates/agent/src/btrfs.rs`):

- **Full index.** `BTRFS_IOC_TREE_SEARCH_V2` with 16 MB of buffers, in parallel slices of the
  inode numbers (a slice starts at an inode, so an inode's items stay together). An inode's
  INODE_ITEM (mode, size, mtime, otime = date created) comes right before its INODE_REF items
  (a name in a parent folder; several for hard links; INODE_EXTREF when a folder holds too many
  names of one inode for one item), so one pass gives every entry, with no readdir or lstat per
  file. A DIR_INDEX item pointing at a ROOT_ITEM is a nested subvolume or snapshot: its tree is
  read too and hung under that name (its device comes from an lstat of its folder). If the
  ROOT_REF doesn't name that folder, it's the empty stand-in a snapshot shows for a subvolume it
  didn't copy, and gets an lstat. Inodes nobody refers to (orphans being deleted) aren't
  reachable and drop out; the subvolume root's ".." ref to itself is skipped. A test on a btrfs
  image compares this with a walk entry by entry (path, attributes, size, dates, device, inode)
  with hard links past one INODE_REF item, a fifo, symlinks, non-ASCII names, a nested
  subvolume and a snapshot; only the stand-in's dates differ, because the kernel makes them up
  at every look.
- **Catch-up by generation.** Each subvolume's generation (its ROOT_ITEM's) is read *before*
  its tree, so anything changing during the read is newer. Later a search with `min_transid =
  generation + 1` returns only the leaves written since (btrfs skips every unchanged subtree),
  and among their INODE_ITEMs those whose own transid is newer: every changed file and folder.
  A changed file just gets its new metadata. Deletions leave no item to find, but adding,
  removing or renaming an entry always rewrites its folder's inode, so each changed folder is
  listed once (readdir, which also gives inode numbers; no lstat) and compared with the tree:
  names gone are removed, new names are looked up, and a new name whose inode is one that left
  its old place is a move (it keeps its id; a folder keeps its contents). Subvolumes created
  inside a share are followed from then on. A test makes those changes (new, deleted, grown and
  renamed entries, new hard links, a new and a deleted subvolume) and compares with a walk.
- **btrfs's own lag.** New entries and sizes reach the trees at btrfs's next commit (up to
  30 s; a new file's size and extents even at writeback), while an lstat sees them at once. So a
  read can trail the very latest changes; the catch-up every minute picks those up, and the live
  events (lstat) cover files written after their folder is followed. The agent doesn't force
  commits.
- **Kept across restarts.** The index is saved in `/config` (every 10 minutes when it changed,
  after the first index, and on SIGTERM, i.e. `docker stop`), with its epoch, ids, sequence
  number and generations. At start it's loaded before the server listens, then caught up (or
  re-walked where there's no btrfs). A PC in step with the save resumes and receives only the
  changes; sequence numbers continue 2^40 above the saved one, so a PC that had seen ops after
  the save (which the agent lost) gets a full transfer instead of a mix of two histories, and a
  resuming PC is told the new number at once.

Measured on the test NAS (`quickseek-agent bench`, read-only, idle I/O priority, 2 threads; the cloud
sync was paused, I/O pressure 5-40 %): `data`, 708,473 entries: warm, the tree search took 5.5 s
and the walk 13.6-13.8 s; the first, partly cold run 45.6 s against 27.0 s for a walk right after
it (the shares are mostly big files, and the tree search also reads every file-extent item, which
a walk never touches, so cold it can lose). A catch-up with nothing changed: 6-38 ms. `media`,
12,999 entries: 0.2 s warm either way. In the agent: the first index of `data` 8.2 s, `media`
1.5 s; a `docker restart` was back with both shares loaded within about a second, and the PC
resumed without a full transfer; a file written while the agent was down was searchable on the
PC 7.1 s after `docker start` (container start included), the catch-up applying 1,528 changed
inodes from that downtime. In WSL on a fast virtual disk (590k synthetic files, cache dropped
before each first read): 5.4 s against 8.3 s for the walk, warm 1.1 s against 1.7 s.

## Protocol
The brief suggested reusing the IPC request/response types (`Search`, `Fetch`, `Tree`, ...) over
TLS, with the service forwarding queries. Rejected after reading `ipc.rs` and `engine.rs`:
forwarding means a network round trip per keystroke and per scrolled page, merging sorted pages
from two sources, a remote variant of the folder-size tree (`tree.rs` works on `Index`), remote
history comparisons and still a local copy for offline use. Replicating the index instead
reuses all of that unchanged, and 590k entries are only about 6 MB compressed.

What is reused is the IPC's framing and versioning conventions: every message is a `u32`
little-endian length and a body; the first exchange is a hello naming the protocol range and
build, both sides pick the newest common version, and an unknown version is a clean error. The
protocol lives in a small shared crate, `quickseek-remote`, used by both the agent and the
service, so the two can't drift.

Messages (version 1):

| Direction | Message | Purpose |
|---|---|---|
| PC → agent | `Hello { min, max, build }` | version handshake (agent answers `Hello` with the chosen version and its build) |
| PC → agent | `Pair { code_proof, nonce, name }` / `Auth { token }` | pairing, or authenticating a paired PC |
| agent → PC | `Paired { token, agent_proof }` / `Welcome { agent_id, shares }` | |
| PC → agent | `Subscribe { share, epoch, seq }` | stream one share, resuming after `seq` of `epoch` if possible |
| agent → PC | `Full { epoch, seq, count }` + `Chunk { bytes }`… + `FullEnd` | the whole tree, zstd-compressed varint records, 1 MB chunks |
| agent → PC | `Changes { seq, ops }` | `Create {id, parent, name, kind, size, mtime, ctime}`, `Remove {id}`, `Move {id, parent, name}`, `Update {id, size, mtime, ctime}` |
| agent → PC | `Disk { capacity, free }`, `State { text }` | for the tree's volume row and the status |
| PC → agent | `History { since }` | agent answers `Snapshots { list }` with its daily snapshots after `since` |
| both | `Ping` / `Pong` | keep-alive every 30 s; a connection silent for 90 s is dropped |

**Ids.** The agent numbers entries densely (root = 5, the first user id = 24, like NTFS record
numbers) and never reuses one within an *epoch* (a random 64-bit value chosen at agent start).
On the PC these ids are the index's record numbers, so the existing record-based mutations
(`add_link`, `remove_record`, `rename_entry`, `set_file_info`) apply changes in O(1) without path
lookups, exactly like USN journal records on NTFS. When the agent restarts (new epoch) or the
PC's `seq` has fallen out of the agent's change log (the last 200,000 ops), the PC gets a full
transfer; otherwise only the missing ops.

**Times and attributes.** Times are sent as Windows FILETIMEs (converted on the agent).
Directories get `FILE_ATTRIBUTE_DIRECTORY`; names starting with a dot get
`FILE_ATTRIBUTE_HIDDEN` (what Samba shows by default); symlinks are entries with
`FILE_ATTRIBUTE_REPARSE_POINT` and are not followed.

## Pairing and authentication
TLS 1.3 (rustls, no OpenSSL), with the agent's self-signed certificate (generated on first
start, kept in `/config`). There is no CA: the PC **pins the certificate's SHA-256 fingerprint**
at pairing time and refuses any other certificate later.

Pairing must not let a machine in the middle substitute its own certificate, so the pairing code
authenticates the fingerprint:

1. The agent logs a one-time code (`K7QF-3MZD`, 40 bits, 15 minutes, 5 tries) when nobody is
   paired yet, and `quickseek-agent pair` (or `docker exec … quickseek-agent pair`) prints a
   new one. UGOS shows container logs in its Docker app, so no shell is needed.
2. `quickseek remotes add mynas.local --pair K7QF-3MZD` connects, reads the certificate the
   agent presented and sends `Pair { proof = HMAC-SHA256(code, "pc" ‖ fingerprint ‖ nonce) }`.
3. The agent recomputes the proof with *its own* fingerprint. A man in the middle presents a
   different certificate, so its proof can't match; five misses burn the code.
4. The agent answers with a random 256-bit token and its own proof
   (`HMAC(code, "agent" ‖ fingerprint ‖ nonce)`), so the PC knows it talked to the holder of the
   code. The agent stores only the token's SHA-256 (with the PC's name) in `/config/clients.json`.
5. Later connections: pinned certificate + `Auth { token }`. `quickseek-agent clients` lists
   paired PCs, `quickseek-agent revoke <name>` removes one.

On the PC the token lives in `<db>\remote\<host>.json`, not in `config.json` (which the service
hands to every client over the pipe); the host, port, fingerprint and share mapping are in
`config.json`.

## The Windows side
- **Configuration**: `config.json` gains `remotes: [{ host, port, fingerprint, shares: [{ name,
  root, unc }] }]`. `quickseek remotes add <host> [--port] --pair <code>` asks the service to
  pair (new IPC requests in protocol 5), `quickseek remotes list` shows each share with its state,
  `quickseek remotes remove <host>` forgets it (and revokes nothing on the agent: that's the
  agent's `revoke`). Options → Index → *NAS agents* does the same in the GUI.
- **Volumes**: each share is a `VolumeKind::Remote` volume keyed by its UNC root
  (`\\mynas.local\data`), so paths look exactly like the SMB paths they correspond to and
  opening, previewing or deleting a file goes through SMB as before. The agent's root
  (`/volume1/data`) maps to the UNC root; the agent reports the share name, and the default UNC
  root is `\\<host>\<share>`. A remote volume replaces a folder index with the same key (the
  folder entry stays in the config and comes back if the remote is removed).
- **Cache**: the replica is saved like a folder index (`<db>\remote\<key>.idx`, same format),
  with the agent's epoch and seq in the journal-id and next-USN fields. At start the service
  serves the cache (`cached`) and resumes from its seq.
- **States** (shown in `quickseek status`, the GUI and `disk_overview`): `connecting`,
  `receiving` (full transfer), `ready` (live), `cached` (serving the saved copy, agent not
  reachable yet), `offline` (was connected, lost it; retrying with backoff 1 s → 60 s),
  `pairing needed` (the agent rejected the token), `certificate changed` (pin mismatch: never
  connects until re-paired).
- **Disk space**: the volume row's capacity and free space come from the agent's `statvfs`, not
  from SMB.

## History
The PC's daily snapshot already includes remote volumes (they are ordinary indexes), so on days
the PC runs nothing changes. For the other days the agent takes its own snapshot each day at
local midnight + 5 min (folders ≥ 10 MB and files ≥ 10 MB per share, the same thresholds as
`history.rs`), keeps 400 days, and on connect the PC asks for those newer than its last import.
Each imported day is merged into the PC's snapshot of that local day if there is one that lacks
the share, or saved as a snapshot holding only that share if the PC has none that day. Growth
comparisons and series then cover the NAS even across weeks the PC was off.

## Resource limits on the NAS
- Memory: on the test NAS, 721k entries in two shares take 112-115 MB once indexed (the tree with
  each entry's device and inode, the saved-index buffers, the change log of up to 200k ops). A
  first walk of 640k entries peaked at 152 MB (the first index records no ops: every PC gets
  it in full anyway). The compose file sets `mem_limit: 256m`.
- CPU: the walk uses at most 4 threads (`--threads`, default min(4, cores)); live tracking is one
  thread mostly asleep. Compose sets `cpus: 1.0` and the process runs at `nice 10`. Walks after
  the first read at idle I/O priority (`ioprio_set` class idle, per walker thread), so Plex and
  Samba win any contention; the first walk (a one-time cost, which the PC is waiting for) and the
  reads behind live changes use normal priority. Under mq-deadline an idle-class read waits until
  the disk is otherwise free or for its 10 s aging, so on a NAS whose disks are saturated (the test NAS
  during a cloud sync: I/O pressure 80-90 %) an idle first walk crawls and an idle `lstat` would
  delay live changes by seconds.
- I/O: the safety re-walk runs once a day (configurable, 0 = never); there is no polling.
- Network: one TLS connection per share and PC; idle traffic is a ping every 30 s.
- Privileges: `CAP_SYS_ADMIN` (fanotify) and `CAP_DAC_READ_SEARCH` (read every directory), a
  read-only mount of the volume, a read-only root filesystem, `no-new-privileges`, everything
  else dropped. Without `CAP_SYS_ADMIN` it runs with inotify.

## Not in this version
- Content search reads files over SMB as before (the agent doesn't serve file contents).
- No write operations through the agent (deletes go through SMB with the user's credentials).
- One agent per host; several NAS volumes are several roots of one agent.
