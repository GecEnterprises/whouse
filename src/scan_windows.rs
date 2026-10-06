//! Windows path sampling through disk handles and file-position deltas.
use super::*;
use crate::windows::{self, Handle};
use std::collections::{HashMap, HashSet};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::time::Instant;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_DUP_HANDLE, PROCESS_QUERY_LIMITED_INFORMATION,
};

struct Memory {
    identity: u64,
    files: HashMap<usize, u64>,
    read: u64,
    write: u64,
    at: Instant,
    row: ProcActivity,
}
pub struct PathScanner {
    root: PathBuf,
    opts: Options,
    memory: HashMap<u32, Memory>,
    total_read: u64,
    total_write: u64,
}
impl PathScanner {
    pub fn new(root: PathBuf, opts: Options) -> Self {
        Self {
            root,
            opts,
            memory: HashMap::new(),
            total_read: 0,
            total_write: 0,
        }
    }
    pub fn scan(&mut self) -> Snapshot {
        let started = Instant::now();
        let (readings, scanned, mut denied) = windows::readings(None);
        let probe = std::env::current_exe()
            .ok()
            .and_then(|p| std::fs::File::open(p).ok());
        let (all_handles, warning) = match windows::handles() {
            Ok(h) => (h, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        // Infer the OS's file object type from a known file handle: type indices
        // are not stable across Windows releases. Skip non-file handles early.
        let file_kind = probe
            .as_ref()
            .and_then(|f| {
                all_handles.iter().find(|h| {
                    h.pid == std::process::id() as usize && h.value == f.as_raw_handle() as usize
                })
            })
            .map(|h| h.kind);
        let mut by_pid: HashMap<u32, Vec<windows::SystemHandle>> = HashMap::new();
        let mut holders: HashMap<usize, HashSet<usize>> = HashMap::new();
        for h in all_handles {
            if file_kind.is_some_and(|kind| h.kind != kind) {
                continue;
            }
            holders.entry(h.object).or_default().insert(h.pid);
            if let Ok(pid) = u32::try_from(h.pid) {
                by_pid.entry(pid).or_default().push(h);
            }
        }
        let mut totals = Totals::default();
        let mut rows = Vec::new();
        let mut folders: HashMap<PathBuf, FolderActivity> = HashMap::new();
        let mut global = HashSet::new();
        let mut live = HashSet::new();
        for r in readings {
            if r.pid == std::process::id() {
                continue;
            }
            // SAFETY: OpenProcess returns an owned handle or null.
            let Some(process) = Handle::new(unsafe {
                OpenProcess(
                    PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    r.pid,
                )
            }) else {
                denied += 1;
                continue;
            };
            let prev = self.memory.get(&r.pid).filter(|m| m.identity == r.identity);
            let secs = prev.map_or(1.0, |m| {
                started.duration_since(m.at).as_secs_f64().max(0.001)
            });
            let mut files: Vec<OpenFile> = Vec::new();
            let mut positions = HashMap::new();
            let mut seen: HashMap<usize, usize> = HashMap::new();
            for h in by_pid.remove(&r.pid).unwrap_or_default() {
                let Some(f) = windows::file_handle(&process, h) else {
                    continue;
                };
                if !under_root(&self.root, &f.path) {
                    continue;
                }
                if let Some(&index) = seen.get(&h.object) {
                    files[index].fds.push(h.value as u32);
                    continue;
                }
                let shared = holders.get(&h.object).is_some_and(|p| p.len() > 1);
                let access = match (h.access & 1 != 0, h.access & (2 | 4) != 0) {
                    (true, false) => Access::Read,
                    (false, true) => Access::Write,
                    _ => Access::ReadWrite,
                };
                let delta = if f.dir {
                    0
                } else {
                    f.pos
                        .zip(prev.and_then(|m| m.files.get(&h.object).copied()))
                        .map_or(0, |(b, a)| b.saturating_sub(a))
                };
                let (read, write) = match access {
                    Access::Read => (delta, 0),
                    Access::Write => (0, delta),
                    Access::ReadWrite => {
                        let (dr, dw) = prev.map_or((0, 0), |m| {
                            (
                                r.read.saturating_sub(m.read),
                                r.write.saturating_sub(m.write),
                            )
                        });
                        let frac = if dr as f64 + dw as f64 > 0.0 {
                            dr as f64 / (dr as f64 + dw as f64)
                        } else {
                            0.5
                        };
                        let read = (delta as f64 * frac) as u64;
                        (read, delta - read)
                    }
                };
                if let Some(pos) = f.pos {
                    positions.insert(h.object, pos);
                }
                let folder = if f.dir {
                    f.path.clone()
                } else {
                    f.path.parent().unwrap_or(&self.root).to_path_buf()
                };
                let acc = folders
                    .entry(folder.clone())
                    .or_insert_with(|| FolderActivity {
                        path: folder,
                        ..FolderActivity::default()
                    });
                if !acc.pids.contains(&r.pid) {
                    acc.pids.push(r.pid);
                }
                if f.dir {
                    acc.opened += 1;
                    totals.dirs += 1;
                } else {
                    acc.files += 1;
                    totals.files += 1;
                }
                if global.insert(h.object) {
                    self.total_read += read;
                    self.total_write += write;
                    totals.read_bps += read as f64 / secs;
                    totals.write_bps += write as f64 / secs;
                    acc.read_bps += read as f64 / secs;
                    acc.write_bps += write as f64 / secs;
                }
                seen.insert(h.object, files.len());
                files.push(OpenFile {
                    fds: vec![h.value as u32],
                    path: f.path,
                    kind: if f.dir {
                        Kind::Dir
                    } else if h.access & 7 == 0 {
                        Kind::Other
                    } else {
                        Kind::File
                    },
                    access,
                    pos: f.pos.unwrap_or(0),
                    size: f.size,
                    read_bps: read as f64 / secs,
                    write_bps: write as f64 / secs,
                    deleted: false,
                    shared,
                });
            }
            if files.is_empty() {
                continue;
            }
            let read_bps: f64 = files.iter().map(|f| f.read_bps).sum();
            let write_bps: f64 = files.iter().map(|f| f.write_bps).sum();
            let io = prev.map(|m| ProcIo {
                rchar_bps: r.read.saturating_sub(m.read) as f64 / secs,
                wchar_bps: r.write.saturating_sub(m.write) as f64 / secs,
                ..ProcIo::default()
            });
            let row = ProcActivity {
                pid: r.pid,
                identity: r.identity,
                ppid: r.ppid,
                comm: r.name,
                cmdline: r.command,
                user: r.user,
                state: r.state,
                cwd: None,
                focus: files
                    .first()
                    .and_then(|f| f.path.parent().map(Path::to_path_buf)),
                read_total: prev.map_or(0, |m| m.row.read_total) + (read_bps * secs) as u64,
                write_total: prev.map_or(0, |m| m.row.write_total) + (write_bps * secs) as u64,
                hint_read_bps: io
                    .filter(|_| files.iter().any(|f| !matches!(f.access, Access::Write)))
                    .map_or(0.0, |io| (io.rchar_bps - read_bps).max(0.0)),
                hint_write_bps: io
                    .filter(|_| files.iter().any(|f| !matches!(f.access, Access::Read)))
                    .map_or(0.0, |io| (io.wchar_bps - write_bps).max(0.0)),
                files,
                files_observed: true,
                read_bps,
                write_bps,
                io,
                idle_for: None,
            };
            live.insert(r.pid);
            totals.procs += 1;
            self.memory.insert(
                r.pid,
                Memory {
                    identity: r.identity,
                    files: positions,
                    read: r.read,
                    write: r.write,
                    at: started,
                    row: row.clone(),
                },
            );
            rows.push(row);
        }
        self.memory
            .retain(|pid, m| live.contains(pid) || started.duration_since(m.at) < self.opts.linger);
        for (&pid, m) in &self.memory {
            if !live.contains(&pid) {
                rows.push(ProcActivity {
                    read_bps: 0.0,
                    write_bps: 0.0,
                    hint_read_bps: 0.0,
                    hint_write_bps: 0.0,
                    files: Vec::new(),
                    files_observed: false,
                    io: None,
                    idle_for: Some(started.duration_since(m.at)),
                    ..m.row.clone()
                });
            }
        }
        let mut folders: Vec<_> = folders.into_values().collect();
        sort_folders(&mut folders);
        totals.read_total = self.total_read;
        totals.write_total = self.total_write;
        Snapshot {
            procs: rows,
            folders,
            totals,
            scanned,
            denied,
            scan_time: started.elapsed(),
            mode: Mode::Path,
            warning,
        }
    }
}

fn under_root(root: &Path, path: &Path) -> bool {
    let root = root.to_string_lossy().replace('/', "\\").to_lowercase();
    let path = path.to_string_lossy().replace('/', "\\").to_lowercase();
    let root = root.trim_end_matches('\\');
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|tail| tail.starts_with('\\'))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_match_case_insensitively_at_component_boundaries() {
        assert!(under_root(
            Path::new(r"\\?\C:\Data"),
            Path::new(r"\\?\c:\DATA\file")
        ));
        assert!(!under_root(
            Path::new(r"\\?\C:\Data"),
            Path::new(r"\\?\C:\Database\file")
        ));
    }
    #[test]
    fn finds_a_live_disk_handle() {
        use std::io::Write;
        use std::os::windows::io::AsRawHandle;
        let path = std::env::temp_dir().join(format!("whouse-handle-{}", std::process::id()));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"example").unwrap();
        let entry = windows::handles()
            .unwrap()
            .into_iter()
            .find(|h| {
                h.pid == std::process::id() as usize && h.value == file.as_raw_handle() as usize
            })
            .unwrap();
        let process =
            Handle::new(unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, std::process::id()) }).unwrap();
        let info = windows::file_handle(&process, entry).unwrap();
        assert_eq!(info.pos, Some(7));
        assert_eq!(info.size, 7);
        assert_eq!(info.path, std::fs::canonicalize(&path).unwrap());
        drop(file);
        std::fs::remove_file(path).unwrap();
    }
}
