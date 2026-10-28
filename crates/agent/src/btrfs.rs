//! The btrfs fast path: read a share's index straight out of the
//! filesystem's trees, as Quickseek reads NTFS's MFT, instead of a readdir
//! and an lstat per entry; and catch up after a restart or lost events by
//! asking btrfs what changed since a generation.
//!
//! **Full index.** `BTRFS_IOC_TREE_SEARCH_V2` (needs `CAP_SYS_ADMIN`) over
//! the share's subvolume tree, 16 MB of items per call, in key order. Each
//! inode's INODE_ITEM (mode, size, times) comes right before its INODE_REF
//! items (a name in a parent folder, several for hard links; INODE_EXTREF
//! when a folder has too many names for one item), so one pass gives every
//! entry. A DIR_INDEX item pointing at a ROOT_ITEM is a nested subvolume
//! (or snapshot): its tree is read the same way and hung under that name.
//! If the subvolume's ROOT_REF doesn't name that folder, it's the empty
//! placeholder a snapshot shows for a subvolume it didn't copy, and gets an
//! lstat like the walk would. Inodes nobody refers to any more (orphans
//! being deleted) aren't reachable and drop out. The result is a [`Fresh`],
//! exactly what a walk gives (the tests compare the two entry by entry).
//!
//! **Catch-up.** Each subvolume's generation (from its ROOT_ITEM) is read
//! before indexing. Later a search with `min_transid = generation + 1`
//! returns only the tree leaves written since, and among them the inodes
//! whose own transid is newer: every changed file and folder. A changed
//! file just gets its new metadata. Deletions leave no item to find, but
//! adding, removing or renaming an entry always rewrites its folder's
//! inode, so every changed folder is listed (one readdir, no lstat) and
//! compared with the tree: names gone are removed, new names are looked
//! up, and a new name whose inode is one that went missing elsewhere is a
//! move (it keeps its id, and a folder its contents). See
//! [`crate::share::Share::catch_up`].

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use quickseek_remote::filetime;

use crate::tree::{Fresh, FreshItem, Meta, attrs_of};

/// `_IOWR(0x94, 17, struct btrfs_ioctl_search_args_v2)` (112 bytes).
const IOC_TREE_SEARCH_V2: u64 = 0xC070_9411;
/// Bytes of items per search call (the kernel's limit for v2 is 16 MB).
const BUF: usize = 16 << 20;
const ROOT_TREE: u64 = 1;
const FIRST_FREE_OBJECTID: u64 = 256;
/// Objectids above this are btrfs's own (orphan items, free space, ...).
const LAST_FREE_OBJECTID: u64 = u64::MAX - 256;

const INODE_ITEM: u8 = 1;
const INODE_REF: u8 = 12;
const INODE_EXTREF: u8 = 13;
const DIR_INDEX: u8 = 96;
const ROOT_ITEM: u8 = 132;
const ROOT_REF: u8 = 156;

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

/// A folder as (subvolume tree, inode).
type Folder = (u64, u64);

/// An inode changed since the last catch-up.
#[derive(Debug, Clone, Copy)]
pub struct Changed {
    pub dev: u64,
    pub ino: u64,
    pub inode: Inode,
}

/// One subvolume of a share and the generation its index is current to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subvol {
    pub tree: u64,
    pub dev: u64,
    pub generation: u64,
}

/// What an inode item says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Inode {
    pub transid: u64,
    pub size: u64,
    pub mode: u32,
    pub mtime: i64,
    pub otime: i64,
}

impl Inode {
    fn parse(d: &[u8]) -> Option<Inode> {
        if d.len() < 160 {
            return None;
        }
        let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
        let time = |o: usize| filetime(u64_at(o) as i64, u32_at(o + 8));
        Some(Inode {
            transid: u64_at(8),
            size: u64_at(16),
            mode: u32_at(52),
            // atime at 112, ctime 124, mtime 136, otime (birth) 148.
            mtime: time(136),
            otime: time(148),
        })
    }

    pub fn meta(&self, name: &str, dev: u64, ino: u64) -> Meta {
        let kind = self.mode & S_IFMT;
        Meta {
            attrs: attrs_of(name, kind == S_IFDIR, kind == S_IFLNK),
            size: if kind == S_IFREG { self.size } else { 0 },
            modified: self.mtime,
            created: self.otime,
            dev,
            ino,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
}

#[repr(C)]
struct SearchKey {
    tree_id: u64,
    min_objectid: u64,
    max_objectid: u64,
    min_offset: u64,
    max_offset: u64,
    min_transid: u64,
    max_transid: u64,
    min_type: u32,
    max_type: u32,
    nr_items: u32,
    unused: u32,
    unused1: [u64; 4],
}

/// One item a search returned.
pub struct Item<'a> {
    pub transid: u64,
    pub objectid: u64,
    pub offset: u64,
    pub kind: u8,
    pub data: &'a [u8],
}

pub struct Dir {
    fd: i32,
}

impl Drop for Dir {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

impl Dir {
    pub fn open(p: &Path) -> io::Result<Dir> {
        let c = CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Dir { fd })
    }

    /// Every item of `tree` with a key in [min, max] (compound keys, as
    /// btrfs compares them) in leaves written at or after `min_transid`.
    pub fn search(
        &self,
        tree: u64,
        min: (u64, u8, u64),
        max: (u64, u8, u64),
        min_transid: u64,
        each: impl FnMut(&Item),
    ) -> io::Result<()> {
        self.search_with(tree, min, max, min_transid, BUF, u32::MAX, each)
    }

    /// [`Dir::search`] with `buf_len` bytes per call and at most `limit`
    /// items in all.
    #[allow(clippy::too_many_arguments)]
    fn search_with(
        &self,
        tree: u64,
        min: (u64, u8, u64),
        max: (u64, u8, u64),
        min_transid: u64,
        buf_len: usize,
        mut limit: u32,
        mut each: impl FnMut(&Item),
    ) -> io::Result<()> {
        let mut buf = vec![0u64; (112 + buf_len) / 8];
        let mut cur = min;
        loop {
            {
                let key = unsafe { &mut *(buf.as_mut_ptr() as *mut SearchKey) };
                *key = SearchKey {
                    tree_id: tree,
                    min_objectid: cur.0,
                    max_objectid: max.0,
                    min_offset: cur.2,
                    max_offset: max.2,
                    min_transid,
                    max_transid: u64::MAX,
                    min_type: cur.1 as u32,
                    max_type: max.1 as u32,
                    nr_items: limit,
                    unused: 0,
                    unused1: [0; 4],
                };
                buf[13] = buf_len as u64; // buf_size, right after the 104-byte key
            }
            let r = unsafe { libc::ioctl(self.fd, IOC_TREE_SEARCH_V2 as _, buf.as_mut_ptr()) };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
            let n = unsafe { (*(buf.as_ptr() as *const SearchKey)).nr_items };
            if n == 0 {
                return Ok(());
            }
            limit = limit.saturating_sub(n);
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts((buf.as_ptr() as *const u8).add(112), buf_len)
            };
            let mut off = 0;
            let mut last = cur;
            for _ in 0..n {
                let h = &bytes[off..off + 32];
                let rd = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().unwrap());
                let len = u32::from_le_bytes(h[28..32].try_into().unwrap()) as usize;
                let item = Item {
                    transid: rd(0),
                    objectid: rd(8),
                    offset: rd(16),
                    kind: u32::from_le_bytes(h[24..28].try_into().unwrap()) as u8,
                    data: &bytes[off + 32..off + 32 + len],
                };
                last = (item.objectid, item.kind, item.offset);
                each(&item);
                off += 32 + len;
            }
            // Continue right after the last key.
            cur = match last {
                (o, t, u64::MAX) if t == u8::MAX => (o + 1, 0, 0),
                (o, t, u64::MAX) => (o, t + 1, 0),
                (o, t, f) => (o, t, f + 1),
            };
            if limit == 0 || cur > max || last.0 == u64::MAX && last.2 == u64::MAX {
                return Ok(());
            }
        }
    }

    /// Whether `tree` has an item with objectid in [from, to].
    fn any_in(&self, tree: u64, from: u64, to: u64) -> io::Result<bool> {
        let mut found = false;
        self.search_with(
            tree,
            (from, 0, 0),
            (to, u8::MAX, u64::MAX),
            0,
            64 << 10,
            1,
            |_| found = true,
        )?;
        Ok(found)
    }

    /// The highest inode number in use in `tree` (special objectids near
    /// the top of the range, like orphan items, aside).
    fn highest_inode(&self, tree: u64) -> io::Result<u64> {
        let (mut lo, mut hi) = (FIRST_FREE_OBJECTID, LAST_FREE_OBJECTID);
        if !self.any_in(tree, lo, hi)? {
            return Ok(lo);
        }
        // Invariant: something at or above lo, nothing above hi.
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if self.any_in(tree, mid, LAST_FREE_OBJECTID)? {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        Ok(lo)
    }

    /// The subvolume (tree id) this folder is in.
    pub fn tree_id(&self) -> io::Result<u64> {
        // BTRFS_IOC_INO_LOOKUP with tree id 0 fills in the folder's tree.
        #[repr(C)]
        struct InoLookup {
            treeid: u64,
            objectid: u64,
            name: [u8; 4080],
        }
        const IOC_INO_LOOKUP: u64 = 0xD000_9412;
        let mut a = InoLookup {
            treeid: 0,
            objectid: FIRST_FREE_OBJECTID,
            name: [0; 4080],
        };
        let r = unsafe { libc::ioctl(self.fd, IOC_INO_LOOKUP as _, &mut a as *mut InoLookup) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(a.treeid)
    }

    /// A subvolume's generation: the last transaction that changed it.
    pub fn generation(&self, tree: u64) -> io::Result<u64> {
        let mut generation = None;
        self.search(
            ROOT_TREE,
            (tree, ROOT_ITEM, 0),
            (tree, ROOT_ITEM, u64::MAX),
            0,
            |it| {
                if generation.is_none() && it.data.len() >= 168 {
                    generation = Some(u64::from_le_bytes(it.data[160..168].try_into().unwrap()));
                }
            },
        )?;
        generation.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no root item for subvolume {tree}"),
            )
        })
    }

    /// Whether subvolume `child` is linked from folder `dir` of `parent` as `name`.
    fn root_ref(&self, parent: u64, child: u64, dir: u64, name: &[u8]) -> io::Result<bool> {
        let mut found = false;
        self.search(
            ROOT_TREE,
            (parent, ROOT_REF, child),
            (parent, ROOT_REF, child),
            0,
            |it| {
                let d = it.data;
                if d.len() >= 18 {
                    let dirid = u64::from_le_bytes(d[0..8].try_into().unwrap());
                    let n = u16::from_le_bytes(d[16..18].try_into().unwrap()) as usize;
                    found |= dirid == dir && d.get(18..18 + n) == Some(name);
                }
            },
        )?;
        Ok(found)
    }
}

/// Whether `root` is on btrfs and its trees can be searched here (btrfs,
/// and the right to: CAP_SYS_ADMIN).
pub fn probe(root: &Path) -> io::Result<()> {
    let d = Dir::open(root)?;
    let tree = d.tree_id()?;
    d.generation(tree)?;
    Ok(())
}

/// One entry as the tree search found it: in subvolume `tree`, inode `ino`,
/// named `name` in folder `parent_ino` of the same tree.
struct Raw {
    tree: u64,
    ino: u64,
    parent_ino: u64,
    name: Box<str>,
    inode: Inode,
}

/// What reading the trees found besides entries.
#[derive(Default)]
struct Extras {
    /// (tree, folder, name, nested subvolume) linked there.
    links: Vec<(u64, u64, Box<str>, u64)>,
    /// (tree, folder, name) to lstat: snapshot stand-ins for subvolumes.
    lstat: Vec<(u64, u64, Box<str>)>,
    /// Each tree's root folder inode (256).
    roots: HashMap<u64, Inode>,
}

/// Reads the inodes of subvolume `tree` in `range`, appending its entries
/// to `out`.
fn read_range(
    d: &Dir,
    tree: u64,
    range: (u64, u64),
    buf_len: usize,
    out: &mut Vec<Raw>,
    ex: &mut Extras,
) -> io::Result<()> {
    // An inode's items come in key order: its INODE_ITEM, then its refs.
    let mut cur: Option<(u64, Inode)> = None;
    let push = |out: &mut Vec<Raw>, ino: u64, inode: Inode, parent: u64, name: &[u8]| {
        // A subvolume's root folder refers to itself as "..": not an entry.
        if ino == parent {
            return;
        }
        // Names that aren't UTF-8 are skipped, as the walk does.
        if let Ok(name) = std::str::from_utf8(name) {
            out.push(Raw {
                tree,
                ino,
                parent_ino: parent,
                name: name.into(),
                inode,
            });
        }
    };
    let each = |it: &Item| {
        let data = it.data;
        match it.kind {
            INODE_ITEM => {
                if let Some(i) = Inode::parse(data) {
                    if it.objectid == FIRST_FREE_OBJECTID {
                        ex.roots.insert(tree, i);
                    }
                    cur = Some((it.objectid, i));
                }
            }
            INODE_REF => {
                // { index u64, name_len u16, name }…, the folder in the key's offset.
                let Some((ino, inode)) = cur.filter(|c| c.0 == it.objectid) else {
                    return;
                };
                let mut o = 0;
                while o + 10 <= data.len() {
                    let n = u16::from_le_bytes([data[o + 8], data[o + 9]]) as usize;
                    let Some(name) = data.get(o + 10..o + 10 + n) else {
                        break;
                    };
                    push(out, ino, inode, it.offset, name);
                    o += 10 + n;
                }
            }
            INODE_EXTREF => {
                // { folder u64, index u64, name_len u16, name }…
                let Some((ino, inode)) = cur.filter(|c| c.0 == it.objectid) else {
                    return;
                };
                let mut o = 0;
                while o + 18 <= data.len() {
                    let parent = u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
                    let n = u16::from_le_bytes([data[o + 16], data[o + 17]]) as usize;
                    let Some(name) = data.get(o + 18..o + 18 + n) else {
                        break;
                    };
                    push(out, ino, inode, parent, name);
                    o += 18 + n;
                }
            }
            DIR_INDEX => {
                // { location (objectid u64, type u8, offset u64), transid u64,
                //   data_len u16, name_len u16, type u8, name }
                if data.len() < 30 {
                    return;
                }
                let loc = u64::from_le_bytes(data[0..8].try_into().unwrap());
                let n = u16::from_le_bytes([data[27], data[28]]) as usize;
                let Some(name) = data
                    .get(30..30 + n)
                    .and_then(|n| std::str::from_utf8(n).ok())
                else {
                    return;
                };
                if data[8] == ROOT_ITEM {
                    ex.links.push((tree, it.objectid, name.into(), loc));
                } else if data[8] == INODE_ITEM && loc < FIRST_FREE_OBJECTID {
                    // An inode below 256: the empty stand-in a snapshot shows.
                    ex.lstat.push((tree, it.objectid, name.into()));
                }
            }
            _ => {}
        }
    };
    d.search_with(
        tree,
        (range.0, 0, 0),
        (range.1, u8::MAX, u64::MAX),
        0,
        buf_len,
        u32::MAX,
        each,
    )
}

/// Reads subvolume `tree` with `threads` threads, each searching its own
/// slices of the inode numbers (a slice starts at an inode, so an inode's
/// item and its refs stay together), 16 MB of buffers in all.
fn read_tree(
    d: &Dir,
    tree: u64,
    threads: usize,
    out: &mut Vec<Raw>,
    ex: &mut Extras,
) -> io::Result<()> {
    let threads = threads.max(1);
    let top = d.highest_inode(tree)?;
    // Several slices per thread, so a dense stretch doesn't hold one up.
    let slices = (threads * 8) as u64;
    let width = (top / slices).max(1);
    let ranges: Vec<(u64, u64)> = (0..slices)
        .map(|i| {
            (
                i * width,
                if i + 1 == slices {
                    u64::MAX
                } else {
                    (i + 1) * width - 1
                },
            )
        })
        .filter(|r| r.0 <= top || r.1 == u64::MAX)
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let buf_len = (BUF / threads).max(1 << 20);
    let results: Vec<io::Result<(Vec<Raw>, Extras)>> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| -> io::Result<(Vec<Raw>, Extras)> {
                    let (mut raws, mut extras) = (Vec::new(), Extras::default());
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&r) = ranges.get(i) else {
                            return Ok((raws, extras));
                        };
                        read_range(d, tree, r, buf_len, &mut raws, &mut extras)?;
                    }
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    for r in results {
        let (raws, e) = r?;
        out.extend(raws);
        ex.links.extend(e.links);
        ex.lstat.extend(e.lstat);
        ex.roots.extend(e.roots);
    }
    Ok(())
}

/// The share at `root` read from the btrfs trees: the same listing a walk
/// gives, and the subvolumes it spans with the generation it's current to.
pub fn index(root: &Path, threads: usize) -> io::Result<(Fresh, Vec<Subvol>)> {
    let d = Dir::open(root)?;
    let top = std::fs::symlink_metadata(root)?;
    let first = d.tree_id()?;
    let mut raws = Vec::new();
    let mut ex = Extras::default();
    let mut gens: HashMap<u64, u64> = HashMap::new();
    let mut todo = vec![first];
    while let Some(tree) = todo.pop() {
        if gens.contains_key(&tree) {
            continue;
        }
        // The generation first: whatever changes while the tree is read is
        // newer, so the next catch-up sees it.
        gens.insert(tree, d.generation(tree)?);
        let before = ex.links.len();
        read_tree(&d, tree, threads, &mut raws, &mut ex)?;
        let new: Vec<(u64, u64, Box<str>, u64)> = ex.links.drain(before..).collect();
        for (t, dir, name, child) in new {
            if d.root_ref(t, child, dir, name.as_bytes())? {
                todo.push(child);
                ex.links.push((t, dir, name, child));
            } else {
                // A snapshot's stand-in for a subvolume it didn't copy.
                ex.lstat.push((t, dir, name));
            }
        }
    }
    build(root, &top, first, raws, ex, gens)
}

/// Builds the [`Fresh`] breadth first from the share's folder: only what's
/// reachable from it, with each nested subvolume's device taken from its
/// folder when the walk gets there.
fn build(
    root: &Path,
    top: &std::fs::Metadata,
    first: u64,
    mut raws: Vec<Raw>,
    ex: Extras,
    gens: HashMap<u64, u64>,
) -> io::Result<(Fresh, Vec<Subvol>)> {
    let mut kids: HashMap<(u64, u64), Vec<u32>> = HashMap::new();
    for (i, r) in raws.iter().enumerate() {
        kids.entry((r.tree, r.parent_ino))
            .or_default()
            .push(i as u32);
    }
    let mut links: HashMap<Folder, Vec<(Box<str>, u64)>> = HashMap::new();
    for (t, dir, name, child) in ex.links {
        links.entry((t, dir)).or_default().push((name, child));
    }
    let mut lstat: HashMap<(u64, u64), Vec<Box<str>>> = HashMap::new();
    for (t, dir, name) in ex.lstat {
        lstat.entry((t, dir)).or_default().push(name);
    }
    let mut devs: HashMap<u64, u64> = HashMap::from([(first, top.dev())]);
    let mut items: Vec<(u32, FreshItem)> = vec![(
        0,
        FreshItem {
            parent: 0,
            name: "".into(),
            meta: Meta::of("", top),
        },
    )];
    let mut queue =
        std::collections::VecDeque::from([((first, top.ino()), 0u32, root.to_path_buf())]);
    let add = |items: &mut Vec<(u32, FreshItem)>, parent: u32, name: Box<str>, meta: Meta| {
        let i = items.len() as u32;
        items.push((i, FreshItem { parent, name, meta }));
        i
    };
    while let Some(((tree, ino), idx, path)) = queue.pop_front() {
        let dev = devs[&tree];
        for k in kids.remove(&(tree, ino)).unwrap_or_default() {
            let r = &mut raws[k as usize];
            let meta = r.inode.meta(&r.name, dev, r.ino);
            let (ino, name) = (r.ino, std::mem::take(&mut r.name));
            if meta.walks() {
                queue.push_back(((tree, ino), items.len() as u32, path.join(&*name)));
            }
            add(&mut items, idx, name, meta);
        }
        for (name, child) in links.remove(&(tree, ino)).unwrap_or_default() {
            let full = path.join(&*name);
            let m = std::fs::symlink_metadata(&full)?;
            devs.insert(child, m.dev());
            let inode = ex.roots.get(&child).copied().unwrap_or_default();
            let i = add(
                &mut items,
                idx,
                name.clone(),
                inode.meta(&name, m.dev(), FIRST_FREE_OBJECTID),
            );
            queue.push_back(((child, FIRST_FREE_OBJECTID), i, full));
        }
        for name in lstat.remove(&(tree, ino)).unwrap_or_default() {
            let full = path.join(&*name);
            if let Ok(m) = std::fs::symlink_metadata(&full) {
                // Empty (the stand-in has no contents), so nothing to walk.
                add(&mut items, idx, name.clone(), Meta::of(&name, &m));
            }
        }
    }
    let subvols = gens
        .into_iter()
        .filter_map(|(tree, generation)| {
            devs.get(&tree).map(|&dev| Subvol {
                tree,
                dev,
                generation,
            })
        })
        .collect();
    Ok((Fresh { items }, subvols))
}

/// The inodes of `subvols` changed since their generations, and the
/// generations to remember after applying them.
pub fn changed(root: &Path, subvols: &[Subvol]) -> io::Result<(Vec<Changed>, Vec<Subvol>)> {
    let d = Dir::open(root)?;
    let mut out = Vec::new();
    let mut gens = Vec::with_capacity(subvols.len());
    for s in subvols {
        let generation = match d.generation(s.tree) {
            Ok(g) => g,
            // The subvolume is gone: its folder's parent changed, which
            // removes it from the tree.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if generation > s.generation {
            d.search(
                s.tree,
                (0, 0, 0),
                (u64::MAX, INODE_ITEM, u64::MAX),
                s.generation + 1,
                |it| {
                    if it.kind == INODE_ITEM
                        && let Some(i) = Inode::parse(it.data)
                        && i.transid > s.generation
                    {
                        out.push(Changed {
                            dev: s.dev,
                            ino: it.objectid,
                            inode: i,
                        });
                    }
                },
            )?;
        }
        gens.push(Subvol { generation, ..*s });
    }
    Ok((out, gens))
}

/// A folder's entries: name → (device, inode), as readdir gives them (no lstat).
pub fn list(dir: &Path) -> io::Result<HashMap<String, u64>> {
    let mut out = HashMap::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        use std::os::unix::fs::DirEntryExt;
        if let Ok(n) = e.file_name().into_string() {
            out.insert(n, e.ino());
        }
    }
    Ok(out)
}

/// A subvolume's tree id and generation, for one created inside a share.
pub fn subvol_of(path: &Path) -> io::Result<Subvol> {
    let d = Dir::open(path)?;
    let tree = d.tree_id()?;
    Ok(Subvol {
        tree,
        dev: std::fs::symlink_metadata(path)?.dev(),
        generation: d.generation(tree)?,
    })
}
