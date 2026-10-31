//! How much space deleting a folder would really give back. On APFS, files can share
//! blocks (clones made by cp -c, bun or Xcode), so du counts the same bytes many times.
//! macOS reports each file's private size (bytes only it holds) and, on volumes with
//! clone tracking, which clone family a file belongs to and how many files share it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Debug, Default, PartialEq)]
pub struct CloneInfo {
    /// Bytes only this file holds.
    pub private: Option<u64>,
    /// Files with the same id share data blocks.
    pub clone_id: Option<u64>,
    /// How many files share this file's data, itself included.
    pub clone_refs: Option<u32>,
}

/// Clone details for a file. Only macOS reports them; elsewhere sizes fall back to
/// allocated blocks, so `freed` matches du except for hard links.
pub fn clone_info(path: &Path) -> Option<CloneInfo> {
    #[cfg(target_os = "macos")]
    return apfs::clone_info(path);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        None
    }
}

#[cfg(target_os = "macos")]
mod apfs {
    use super::CloneInfo;
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }

    extern "C" {
        fn getattrlist(path: *const c_char, list: *mut AttrList, buf: *mut c_void, size: usize, options: u32) -> c_int;
    }

    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
    const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
    const ATTR_CMNEXT_CLONEID: u32 = 0x0000_0100;
    const ATTR_CMNEXT_CLONE_REFCNT: u32 = 0x0000_1000;
    const FSOPT_NOFOLLOW: u32 = 0x0000_0001;
    const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x0000_0020;

    pub fn clone_info(path: &Path) -> Option<CloneInfo> {
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut list = AttrList {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: ATTR_CMN_RETURNED_ATTRS,
            volattr: 0,
            dirattr: 0,
            fileattr: 0,
            forkattr: ATTR_CMNEXT_PRIVATESIZE | ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_CLONE_REFCNT,
        };
        let mut buf = [0u8; 64];
        // SAFETY: the list and buffer outlive the call, and the buffer length is passed.
        let r = unsafe {
            getattrlist(
                c.as_ptr(),
                &mut list,
                buf.as_mut_ptr().cast(),
                buf.len(),
                FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED,
            )
        };
        if r != 0 {
            return None;
        }
        // u32 length, then the attribute_set_t of what came back (5 × u32), then each returned
        // attribute in bit order, packed on 4-byte boundaries.
        let returned = u32::from_ne_bytes(buf[20..24].try_into().ok()?);
        let mut at = 24;
        let mut take = |n: usize| {
            let s = &buf[at..at + n];
            at += n;
            s.to_vec()
        };
        let mut info = CloneInfo::default();
        if returned & ATTR_CMNEXT_PRIVATESIZE != 0 {
            info.private = Some(i64::from_ne_bytes(take(8).try_into().ok()?).max(0) as u64);
        }
        if returned & ATTR_CMNEXT_CLONEID != 0 {
            info.clone_id = Some(u64::from_ne_bytes(take(8).try_into().ok()?));
        }
        if returned & ATTR_CMNEXT_CLONE_REFCNT != 0 {
            info.clone_refs = Some(u32::from_ne_bytes(take(4).try_into().ok()?));
        }
        Some(info)
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Size {
    /// Bytes that deleting the whole folder is sure to free.
    pub freed: u64,
    /// What du would say: allocated blocks, each hard-linked file once.
    pub du: u64,
    pub files: u64,
}

struct Family {
    refs: u32,
    seen: u32,
    shared: u64,
}

/// Walk a folder without following links and size it both ways.
pub fn measure(root: &Path) -> Size {
    let mut s = Size::default();
    let mut links = HashSet::new();
    let mut families: HashMap<u64, Family> = HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            let alloc = m.blocks() * 512;
            s.files += 1;
            if m.nlink() > 1 {
                // Other links may live outside the folder, so only du counts these.
                if links.insert((m.dev(), m.ino())) {
                    s.du += alloc;
                }
                continue;
            }
            s.du += alloc;
            match clone_info(&e.path()) {
                Some(CloneInfo {
                    private: Some(p),
                    clone_id,
                    clone_refs,
                }) => {
                    let p = p.min(alloc);
                    s.freed += p;
                    if let (true, Some(id), Some(refs)) = (alloc > p, clone_id, clone_refs) {
                        let f = families.entry(id).or_insert(Family {
                            refs,
                            seen: 0,
                            shared: 0,
                        });
                        f.seen += 1;
                        f.shared = f.shared.max(alloc - p);
                    }
                }
                _ => s.freed += alloc,
            }
        }
    }
    // Blocks shared only among files inside this folder are freed with it, once.
    s.freed += families
        .values()
        .filter(|f| f.refs > 0 && f.seen >= f.refs)
        .map(|f| f.shared)
        .sum::<u64>();
    s
}

/// Measure several folders on all cores.
pub fn measure_all(roots: &[PathBuf]) -> Vec<Size> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(12);
    let next = AtomicUsize::new(0);
    let out: Vec<Mutex<Size>> = roots.iter().map(|_| Mutex::new(Size::default())).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(root) = roots.get(i) else { break };
                *out[i].lock().unwrap() = measure(root);
            });
        }
    });
    out.into_iter().map(|m| m.into_inner().unwrap()).collect()
}

pub fn human(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 10.0 {
        format!("{gb:.0} GB")
    } else if gb >= 0.95 {
        format!("{gb:.1} GB")
    } else if bytes >= 500_000 {
        format!("{:.0} MB", bytes as f64 / 1e6)
    } else {
        format!("{:.0} KB", bytes as f64 / 1e3)
    }
}

// Clones need APFS, so these tests run on macOS only.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::process::Command;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("reap-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn random_file(p: &Path, bytes: usize) {
        let mut data = vec![0u8; bytes];
        let mut x: u32 = 0x9E37_79B9;
        for b in data.iter_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = x as u8;
        }
        fs::write(p, data).unwrap();
    }

    #[test]
    #[ignore]
    fn probe() {
        let d = tmp("probe");
        random_file(&d.join("a"), 1 << 20);
        Command::new("cp")
            .arg("-c")
            .arg(d.join("a"))
            .arg(d.join("b"))
            .status()
            .unwrap();
        for f in ["a", "b"] {
            println!(
                "{f}: {:?} alloc {}",
                clone_info(&d.join(f)),
                fs::metadata(d.join(f)).unwrap().blocks() * 512
            );
        }
        println!("{:?}", measure(&d));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn clones_count_once_and_only_when_every_copy_goes() {
        let root = tmp("clones");
        let (inside, outside) = (root.join("inside"), root.join("outside"));
        fs::create_dir_all(&inside).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let mb = 1u64 << 20;
        random_file(&inside.join("a"), mb as usize);
        let s = measure(&inside);
        assert_eq!((s.du, s.freed), (mb, mb), "a plain file frees what it uses");

        let cp = |from: &Path, to: &Path| {
            assert!(Command::new("cp")
                .arg("-c")
                .arg(from)
                .arg(to)
                .status()
                .unwrap()
                .success())
        };
        cp(&inside.join("a"), &inside.join("b"));
        if clone_info(&inside.join("a")).and_then(|c| c.clone_refs).is_none() {
            eprintln!("this volume has no clone tracking; skipping the clone checks");
            return;
        }
        let s = measure(&inside);
        assert_eq!(s.du, 2 * mb, "du counts the clone twice");
        assert_eq!(s.freed, mb, "deleting both copies frees the data once");

        cp(&inside.join("a"), &outside.join("c"));
        let s = measure(&inside);
        assert_eq!(s.freed, 0, "a copy outside keeps the shared data alive");

        fs::hard_link(outside.join("c"), outside.join("d")).unwrap();
        assert_eq!(measure(&outside).freed, 0, "hard links count only toward du");
        let _ = fs::remove_dir_all(&root);
    }
}
