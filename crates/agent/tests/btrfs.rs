//! The btrfs fast path against the walk, on a real btrfs (a loop image this
//! test makes and mounts; needs root and btrfs-progs, skipped otherwise).
//! The listings must match entry for entry: path, attributes, size, dates,
//! device and inode, with hard links (also past what one INODE_REF item
//! holds), symlinks, a fifo, non-ASCII names, a nested subvolume and a
//! snapshot with the stand-in it shows for that subvolume. Then a catch-up
//! after changes (new, deleted, grown, renamed folders, new hard links, a
//! new and a deleted subvolume) must again match a fresh walk.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

use quickseek_agent::btrfs;
use quickseek_agent::share::Share;
use quickseek_agent::tree::{Fresh, walk};
use quickseek_remote::ROOT_ID;

fn sh(args: &[&str]) -> bool {
    Command::new(args[0])
        .args(&args[1..])
        .output()
        .is_ok_and(|o| {
            if !o.status.success() {
                eprintln!("{args:?}: {}", String::from_utf8_lossy(&o.stderr));
            }
            o.status.success()
        })
}

struct Fs {
    dir: PathBuf,
    mnt: PathBuf,
}

impl Drop for Fs {
    fn drop(&mut self) {
        sh(&["umount", "-l", self.mnt.to_str().unwrap()]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A fresh btrfs, or None when this can't run here.
fn btrfs_fs(tag: &str) -> Option<Fs> {
    if unsafe { geteuid() } != 0 || !sh(&["mkfs.btrfs", "--version"]) {
        eprintln!("skipped: needs root and btrfs-progs");
        return None;
    }
    let dir = std::env::temp_dir().join(format!("qs-btrfs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("mnt")).unwrap();
    let img = dir.join("img");
    let mnt = dir.join("mnt");
    assert!(sh(&["truncate", "-s", "512M", img.to_str().unwrap()]));
    assert!(sh(&["mkfs.btrfs", "-q", "-f", img.to_str().unwrap()]));
    assert!(sh(&[
        "mount",
        "-o",
        "loop",
        img.to_str().unwrap(),
        mnt.to_str().unwrap()
    ]));
    Some(Fs { dir, mnt })
}

unsafe extern "C" {
    fn geteuid() -> u32;
    fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
}

fn subvol(p: &Path) {
    assert!(sh(&["btrfs", "subvolume", "create", p.to_str().unwrap()]));
}

fn write(p: &Path, len: usize) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, vec![b'x'; len]).unwrap();
}

/// One entry as a comparable line. A snapshot's stand-in for a subvolume
/// it didn't copy (inode 2 on btrfs) gets new dates from the kernel at
/// every look, so its dates aren't compared.
fn line(path: &str, m: &quickseek_agent::tree::Meta) -> String {
    let (mtime, ctime) = if m.ino == 2 && m.walks() {
        (0, 0)
    } else {
        (m.modified, m.created)
    };
    format!(
        "{path} attrs={:x} size={} mtime={mtime} ctime={ctime} dev={} ino={}",
        m.attrs, m.size, m.dev, m.ino
    )
}

/// Every entry of a listing as one comparable line, sorted.
fn lines(f: &Fresh) -> Vec<String> {
    let mut paths = vec![String::new(); f.items.len()];
    let mut out = Vec::new();
    for (i, (_, it)) in f.items.iter().enumerate().skip(1) {
        // Parents come before children in both listings' order? Not
        // necessarily: resolve by walking up.
        let mut parts = vec![&*it.name];
        let mut p = it.parent as usize;
        while p != 0 {
            parts.push(&f.items[p].1.name);
            p = f.items[p].1.parent as usize;
        }
        parts.reverse();
        paths[i] = parts.join("/");
        out.push(line(&paths[i], &it.meta));
    }
    out.sort();
    out
}

/// The share's tree as lines, like [`lines`].
fn tree_lines(s: &Share) -> Vec<String> {
    let st = s.state.lock().unwrap();
    let t = &st.tree;
    let mut out: Vec<String> = t
        .records()
        .iter()
        .map(|r| {
            let path = t.path(r.id);
            let rel = path.strip_prefix(&s.root).unwrap().to_string_lossy();
            line(&rel, &t.get(r.id).unwrap().meta)
        })
        .collect();
    out.sort();
    out
}

fn pool() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap()
}

fn diff(a: &[String], b: &[String]) -> String {
    let only_a: Vec<&String> = a.iter().filter(|x| !b.contains(x)).take(10).collect();
    let only_b: Vec<&String> = b.iter().filter(|x| !a.contains(x)).take(10).collect();
    format!("only in the first: {only_a:#?}\nonly in the second: {only_b:#?}")
}

fn populate(share: &Path) {
    write(&share.join("a.txt"), 5);
    write(&share.join("docs/b.txt"), 1000);
    write(&share.join("docs/deep/c.bin"), 70_000);
    write(&share.join("Ågård ñ 日本.txt"), 3);
    write(&share.join(".hidden"), 0);
    std::fs::create_dir_all(share.join("empty")).unwrap();
    std::os::unix::fs::symlink("a.txt", share.join("link")).unwrap();
    std::fs::hard_link(share.join("a.txt"), share.join("docs/hard")).unwrap();
    // Enough long hard-link names in one folder to overflow an INODE_REF
    // item into INODE_EXTREFs.
    std::fs::create_dir_all(share.join("links")).unwrap();
    for i in 0..300 {
        std::fs::hard_link(
            share.join("docs/b.txt"),
            share
                .join("links")
                .join(format!("{i:03}-{}", "n".repeat(120))),
        )
        .unwrap();
    }
    // Many files in one folder: several leaves.
    for i in 0..3000 {
        write(&share.join("many").join(format!("f{i}")), i % 7);
    }
    let fifo = std::ffi::CString::new(share.join("pipe").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { mkfifo(fifo.as_ptr(), 0o644) }, 0);
    // Renamed and deleted before indexing.
    write(&share.join("old/x.txt"), 1);
    std::fs::rename(share.join("old"), share.join("docs/renamed")).unwrap();
    write(&share.join("gone.txt"), 1);
    std::fs::remove_file(share.join("gone.txt")).unwrap();
    // A nested subvolume, and a snapshot of the share (which shows an
    // empty stand-in for the nested one).
    subvol(&share.join("nested"));
    write(&share.join("nested/inside.txt"), 9);
    write(&share.join("nested/sub/more.txt"), 10);
    assert!(sh(&[
        "btrfs",
        "subvolume",
        "snapshot",
        share.to_str().unwrap(),
        share.join("snap").to_str().unwrap()
    ]));
    assert!(sh(&["sync"]));
}

#[test]
fn tree_search_matches_the_walk() {
    let Some(fs) = btrfs_fs("match") else { return };
    let share = fs.mnt.join("share");
    subvol(&share);
    populate(&share);
    let walked = lines(&walk(&share, &pool()));
    let (fresh, subvols) = btrfs::index(&share, 2).unwrap();
    let searched = lines(&fresh);
    assert!(
        walked.len() == searched.len(),
        "{}",
        diff(&walked, &searched)
    );
    assert!(walked == searched, "{}", diff(&walked, &searched));
    assert!(
        walked.iter().any(|l| l.starts_with("snap/nested ")),
        "the stand-in is listed"
    );
    assert!(walked.iter().any(|l| l.starts_with("links/299-")));
    // The share, the nested subvolume and the snapshot.
    assert_eq!(subvols.len(), 3, "{subvols:?}");
    eprintln!("{} entries match", walked.len());
}

#[test]
fn catch_up_matches_a_walk_after_changes() {
    let Some(fs) = btrfs_fs("catchup") else {
        return;
    };
    let share = fs.mnt.join("share");
    subvol(&share);
    populate(&share);
    let p = pool();
    let s = Share::new("share", &share, 1);
    let (fresh, mut subvols) = btrfs::index(&share, 2).unwrap();
    s.merge_walk(fresh, Default::default());
    let renamed_id = {
        let st = s.state.lock().unwrap();
        let docs = st.tree.child(ROOT_ID, "docs").unwrap();
        st.tree.child(docs, "deep").unwrap()
    };
    // Changes while "the agent was down".
    write(&share.join("new.txt"), 42);
    write(&share.join("docs/newdir/n1.txt"), 1);
    std::fs::remove_file(share.join("a.txt")).unwrap(); // still linked as docs/hard
    std::fs::remove_dir_all(share.join("many")).unwrap();
    write(&share.join("docs/b.txt"), 5000); // grown (and its 300 links with it)
    std::fs::rename(share.join("docs/deep"), share.join("moved-deep")).unwrap();
    std::fs::hard_link(share.join("new.txt"), share.join("nested/new-link")).unwrap_or(());
    std::fs::hard_link(share.join("new.txt"), share.join("docs/new-link")).unwrap();
    write(&share.join("nested/sub/more.txt"), 77);
    subvol(&share.join("docs/newsub"));
    write(&share.join("docs/newsub/z.txt"), 3);
    assert!(sh(&[
        "btrfs",
        "subvolume",
        "delete",
        share.join("snap").to_str().unwrap()
    ]));
    assert!(sh(&["sync"]));

    let t0 = std::time::Instant::now();
    let (changed, gens) = btrfs::changed(&share, &subvols).unwrap();
    let ch = s.catch_up(&changed, &p);
    subvols = gens;
    // New subvolumes join the list.
    for (_, path) in &ch.added {
        let m = std::fs::symlink_metadata(path).unwrap();
        use std::os::unix::fs::MetadataExt;
        if m.ino() == 256 && !subvols.iter().any(|s| s.dev == m.dev()) {
            subvols.push(btrfs::subvol_of(path).unwrap());
        }
    }
    eprintln!(
        "catch-up: {} changed inodes in {} ms",
        changed.len(),
        t0.elapsed().as_millis()
    );
    let walked = lines(&walk(&share, &p));
    let caught = tree_lines(&s);
    assert!(walked == caught, "{}", diff(&walked, &caught));
    // The renamed folder kept its id.
    {
        let st = s.state.lock().unwrap();
        assert_eq!(st.tree.child(ROOT_ID, "moved-deep"), Some(renamed_id));
    }
    // Nothing more changed: an empty catch-up; and the new subvolume is followed.
    write(&share.join("docs/newsub/later.txt"), 1);
    assert!(sh(&["sync"]));
    let (changed, _) = btrfs::changed(&share, &subvols).unwrap();
    s.catch_up(&changed, &p);
    let walked = lines(&walk(&share, &p));
    let caught = tree_lines(&s);
    assert!(walked == caught, "{}", diff(&walked, &caught));
}
