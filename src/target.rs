//! Facts about the watched path itself: where it lives and how full that is.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Mount {
    pub point: PathBuf,
    pub fstype: String,
    pub source: String,
}

#[derive(Clone, Copy, Debug)]
pub struct Space {
    pub total: u64,
    pub free: u64,
}

impl Space {
    pub fn used_fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            1.0 - self.free as f64 / self.total as f64
        }
    }
}

pub struct Target {
    pub root: PathBuf,
    pub is_dir: bool,
    pub mount: Option<Mount>,
    pub space: Option<Space>,
    space_at: Instant,
}

impl Target {
    pub fn new(root: PathBuf) -> Self {
        let is_dir = root.is_dir();
        Self {
            mount: find_mount(&root),
            space: space(&root),
            space_at: Instant::now(),
            is_dir,
            root,
        }
    }

    /// Free space barely moves, and `statvfs` can hang on a dead network
    /// mount, so look at it every few seconds rather than on every frame.
    pub fn refresh_space(&mut self) {
        if self.space_at.elapsed() >= Duration::from_secs(5) {
            self.space = space(&self.root);
            self.space_at = Instant::now();
        }
    }

    /// Path relative to the watched root, for compact display.
    pub fn rel(&self, path: &Path) -> String {
        if path == self.root {
            return if self.is_dir {
                ".".into()
            } else {
                self.root
                    .file_name()
                    .map_or_else(|| ".".into(), |n| n.to_string_lossy().into_owned())
            };
        }
        match path.strip_prefix(&self.root) {
            Ok(rel) => format!("./{}", rel.to_string_lossy()),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    }
}

/// The mount `path` lives on: the longest mount point that is a prefix of it,
/// the last one listed if several are stacked on the same point.
pub fn find_mount(path: &Path) -> Option<Mount> {
    let info = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<Mount> = None;
    for line in info.lines() {
        // "id parent maj:min root point opts [optional...] - fstype source superopts"
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let Some(point) = left.split(' ').nth(4).map(unescape) else {
            continue;
        };
        if !path.starts_with(&point) {
            continue;
        }
        let mut right = right.split(' ');
        let (Some(fstype), Some(source)) = (right.next(), right.next()) else {
            continue;
        };
        let depth = |p: &Path| p.components().count();
        if best
            .as_ref()
            .is_none_or(|b| depth(&point) >= depth(&b.point))
        {
            best = Some(Mount {
                point,
                fstype: fstype.to_string(),
                source: unescape_str(source),
            });
        }
    }
    best
}

fn unescape_str(s: &str) -> String {
    String::from_utf8_lossy(unescape(s).as_os_str().as_bytes()).into_owned()
}

/// mountinfo escapes space, tab, newline and backslash as `\040` style octal.
fn unescape(s: &str) -> PathBuf {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = (b[i] == b'\\' && i + 3 < b.len())
            .then(|| {
                std::str::from_utf8(&b[i + 1..i + 4])
                    .ok()
                    .and_then(|d| u8::from_str_radix(d, 8).ok())
            })
            .flatten();
        match octal {
            Some(v) => {
                out.push(v);
                i += 4;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

pub fn space(path: &Path) -> Option<Space> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` a writable statvfs.
    let st = unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        st
    };
    let unit = st.f_frsize as u64;
    Some(Space {
        total: st.f_blocks as u64 * unit,
        free: st.f_bavail as u64 * unit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_octal_sequences() {
        assert_eq!(unescape("/mnt/my\\040disk"), PathBuf::from("/mnt/my disk"));
        assert_eq!(unescape("/plain"), PathBuf::from("/plain"));
        assert_eq!(unescape("/trailing\\04"), PathBuf::from("/trailing\\04"));
    }

    #[test]
    fn finds_a_mount_and_space_for_the_root_filesystem() {
        let m = find_mount(Path::new("/")).expect("no mount for /");
        assert_eq!(m.point, PathBuf::from("/"));
        let s = space(Path::new("/")).expect("statvfs failed");
        assert!(s.total > 0 && s.free <= s.total);
        assert!((0.0..=1.0).contains(&s.used_fraction()));
    }

    #[test]
    fn relative_display() {
        let mut t = Target::new(PathBuf::from("/tmp"));
        assert_eq!(t.rel(Path::new("/tmp")), ".");
        assert_eq!(t.rel(Path::new("/tmp/a/b")), "./a/b");
        assert_eq!(t.rel(Path::new("/elsewhere")), "/elsewhere");
        t.is_dir = false;
        t.root = PathBuf::from("/tmp/x.bin");
        assert_eq!(t.rel(Path::new("/tmp/x.bin")), "x.bin");
    }
}
