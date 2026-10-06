//! Whole-process counters, independent of the watched path and open handles.
use super::{Access, Kind, Mode, OpenFile, ProcActivity, ProcIo, Snapshot, Totals};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub struct Reading {
    pub pid: u32,
    pub identity: u64,
    pub ppid: u32,
    pub name: String,
    pub command: String,
    pub user: String,
    pub state: char,
    pub read: u64,
    pub write: u64,
    pub disk_read: Option<u64>,
    pub disk_write: Option<u64>,
}

pub struct ObservedFile {
    pub key: (u64, u64),
    pub file: OpenFile,
    pub position: Option<u64>,
}

struct Memory {
    reading: Reading,
    at: Instant,
    row: ProcActivity,
    positions: HashMap<(u64, u64), u64>,
}

pub struct ProcessScanner {
    pid: Option<u32>,
    memory: HashMap<u32, Memory>,
    read_total: u64,
    write_total: u64,
}

impl ProcessScanner {
    pub fn new(pid: Option<u32>) -> Self {
        Self {
            pid,
            memory: HashMap::new(),
            read_total: 0,
            write_total: 0,
        }
    }
    pub fn reset(&mut self) {
        self.memory.clear();
        self.read_total = 0;
        self.write_total = 0;
    }
    pub fn scan(&mut self) -> Snapshot {
        let started = Instant::now();
        let (readings, scanned, denied) = readings(self.pid);
        let (files, warning) = open_files(&readings);
        let mut snapshot = self.sample_files(readings, scanned, denied, started, files);
        snapshot.warning = warning;
        snapshot
    }
    #[cfg(test)]
    fn sample(
        &mut self,
        readings: Vec<Reading>,
        scanned: usize,
        denied: usize,
        at: Instant,
    ) -> Snapshot {
        self.sample_files(readings, scanned, denied, at, HashMap::new())
    }
    fn sample_files(
        &mut self,
        readings: Vec<Reading>,
        scanned: usize,
        denied: usize,
        at: Instant,
        mut observations: HashMap<u32, Vec<ObservedFile>>,
    ) -> Snapshot {
        let mut live = HashSet::new();
        let mut totals = Totals::default();
        let mut rows = Vec::new();
        for r in readings {
            live.insert(r.pid);
            let prev = self
                .memory
                .get(&r.pid)
                .filter(|m| m.reading.identity == r.identity);
            let secs = prev.map_or(1.0, |m| at.duration_since(m.at).as_secs_f64().max(0.001));
            let delta_read = prev.map_or(0, |m| r.read.saturating_sub(m.reading.read));
            let delta_write = prev.map_or(0, |m| r.write.saturating_sub(m.reading.write));
            let disk_rate = |now: Option<u64>, old: Option<u64>| {
                now.zip(old)
                    .map_or(0.0, |(b, a)| b.saturating_sub(a) as f64 / secs)
            };
            let io = ProcIo {
                rchar_bps: delta_read as f64 / secs,
                wchar_bps: delta_write as f64 / secs,
                read_bytes_bps: disk_rate(r.disk_read, prev.and_then(|m| m.reading.disk_read)),
                write_bytes_bps: disk_rate(r.disk_write, prev.and_then(|m| m.reading.disk_write)),
            };
            let observed = observations.remove(&r.pid);
            let files_observed = observed.is_some();
            let mut positions = HashMap::new();
            let files: Vec<_> = observed
                .unwrap_or_default()
                .into_iter()
                .map(|mut observation| {
                    let delta = observation
                        .position
                        .zip(prev.and_then(|m| m.positions.get(&observation.key).copied()))
                        .map_or(0, |(b, a)| b.saturating_sub(a));
                    if let Some(pos) = observation.position {
                        positions.insert(observation.key, pos);
                    }
                    let rate = if observation.file.kind == Kind::File {
                        delta as f64 / secs
                    } else {
                        0.0
                    };
                    let fraction = match observation.file.access {
                        Access::Read => 1.0,
                        Access::Write => 0.0,
                        Access::ReadWrite => {
                            if delta_read as f64 + delta_write as f64 > 0.0 {
                                delta_read as f64 / (delta_read as f64 + delta_write as f64)
                            } else {
                                0.5
                            }
                        }
                    };
                    observation.file.read_bps = rate * fraction;
                    observation.file.write_bps = rate * (1.0 - fraction);
                    observation.file
                })
                .collect();
            let row = ProcActivity {
                pid: r.pid,
                identity: r.identity,
                ppid: r.ppid,
                comm: r.name.clone(),
                cmdline: r.command.clone(),
                user: r.user.clone(),
                state: r.state,
                cwd: None,
                files,
                files_observed,
                read_bps: io.rchar_bps,
                write_bps: io.wchar_bps,
                read_total: prev
                    .map_or(0, |m| m.row.read_total)
                    .saturating_add(delta_read),
                write_total: prev
                    .map_or(0, |m| m.row.write_total)
                    .saturating_add(delta_write),
                io: Some(io),
                hint_read_bps: 0.0,
                hint_write_bps: 0.0,
                focus: None,
                idle_for: None,
            };
            self.read_total = self.read_total.saturating_add(delta_read);
            self.write_total = self.write_total.saturating_add(delta_write);
            totals.read_bps += row.read_bps;
            totals.write_bps += row.write_bps;
            totals.procs += 1;
            rows.push(row.clone());
            self.memory.insert(
                r.pid,
                Memory {
                    reading: r,
                    at,
                    row,
                    positions,
                },
            );
        }
        // Keep vanished/inaccessible processes for two minutes so their history
        // can still be inspected. X means no longer observable, not confirmed dead.
        self.memory.retain(|pid, m| {
            live.contains(pid) || at.duration_since(m.at) < Duration::from_secs(120)
        });
        for (&pid, m) in &self.memory {
            if !live.contains(&pid) {
                rows.push(ProcActivity {
                    state: 'X',
                    read_bps: 0.0,
                    write_bps: 0.0,
                    io: None,
                    files: Vec::new(),
                    files_observed: false,
                    idle_for: Some(at.duration_since(m.at)),
                    ..m.row.clone()
                });
            }
        }
        totals.read_total = self.read_total;
        totals.write_total = self.write_total;
        Snapshot {
            procs: rows,
            folders: Vec::new(),
            totals,
            scanned,
            denied,
            scan_time: at.elapsed(),
            mode: Mode::ReadHistory,
            warning: None,
        }
    }
}

#[cfg(windows)]
fn open_files(readings: &[Reading]) -> (HashMap<u32, Vec<ObservedFile>>, Option<String>) {
    crate::windows::open_files(readings)
}

#[cfg(target_os = "linux")]
fn open_files(readings: &[Reading]) -> (HashMap<u32, Vec<ObservedFile>>, Option<String>) {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    let mut result = HashMap::new();
    let mut unavailable = 0;
    for r in readings {
        let Ok(entries) = fs::read_dir(format!("/proc/{}/fd", r.pid)) else {
            unavailable += 1;
            continue;
        };
        let mut files: Vec<ObservedFile> = Vec::new();
        let mut groups: HashMap<((u64, u64), u32, Option<u64>), usize> = HashMap::new();
        for entry in entries.flatten() {
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(path) = fs::read_link(entry.path()) else {
                continue;
            };
            let Ok(meta) = fs::metadata(entry.path()) else {
                continue;
            };
            let info =
                fs::read_to_string(format!("/proc/{}/fdinfo/{fd}", r.pid)).unwrap_or_default();
            let mut position = None;
            let mut flags = 0u32;
            for line in info.lines() {
                if let Some(value) = line.strip_prefix("pos:") {
                    position = value.trim().parse().ok();
                }
                if let Some(value) = line.strip_prefix("flags:") {
                    flags = u32::from_str_radix(value.trim(), 8).unwrap_or(0);
                }
            }
            let key = (meta.dev(), meta.ino());
            // Merge only descriptors whose access and current offset agree.
            let group = (key, flags, position);
            if let Some(&index) = groups.get(&group) {
                files[index].file.fds.push(fd);
                continue;
            }
            groups.insert(group, files.len());
            let kind = if meta.is_dir() {
                Kind::Dir
            } else if meta.is_file() {
                Kind::File
            } else {
                Kind::Other
            };
            let deleted = path.to_string_lossy().ends_with(" (deleted)");
            files.push(ObservedFile {
                key: (meta.ino() ^ meta.dev().rotate_left(32), fd as u64),
                position,
                file: OpenFile {
                    fds: vec![fd],
                    path,
                    kind,
                    access: match flags & 3 {
                        0 => Access::Read,
                        1 => Access::Write,
                        _ => Access::ReadWrite,
                    },
                    pos: position.unwrap_or(0),
                    size: meta.len(),
                    read_bps: 0.0,
                    write_bps: 0.0,
                    deleted,
                    shared: false,
                },
            });
        }
        result.insert(r.pid, files);
    }
    let warning = (unavailable > 0).then(|| format!("File handles unavailable for {unavailable} processes; their I/O counters are still monitored"));
    (result, warning)
}

#[cfg(windows)]
fn readings(pid: Option<u32>) -> (Vec<Reading>, usize, usize) {
    crate::windows::readings(pid)
}

#[cfg(target_os = "linux")]
fn readings(only: Option<u32>) -> (Vec<Reading>, usize, usize) {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    let mut rows = Vec::new();
    let (mut scanned, mut denied) = (0, 0);
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if only.is_some_and(|p| p != pid) {
                continue;
            }
            scanned += 1;
            let read = || -> std::io::Result<Reading> {
                let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
                let io = fs::read_to_string(format!("/proc/{pid}/io"))?;
                let l = stat.find('(').ok_or(std::io::ErrorKind::InvalidData)?;
                let r = stat.rfind(')').ok_or(std::io::ErrorKind::InvalidData)?;
                let fields: Vec<_> = stat[r + 1..].split_whitespace().collect();
                let identity = fields
                    .get(19)
                    .and_then(|v| v.parse().ok())
                    .ok_or(std::io::ErrorKind::InvalidData)?;
                let counter = |key: &str| {
                    io.lines().find_map(|line| {
                        let (k, v) = line.split_once(':')?;
                        (k == key).then(|| v.trim().parse().ok()).flatten()
                    })
                };
                let name = stat[l + 1..r].to_string();
                let command = fs::read(format!("/proc/{pid}/cmdline"))
                    .map(|b| {
                        String::from_utf8_lossy(&b)
                            .replace('\0', " ")
                            .trim()
                            .to_string()
                    })
                    .unwrap_or_else(|_| name.clone());
                let uid = fs::metadata(format!("/proc/{pid}"))?.uid();
                Ok(Reading {
                    pid,
                    identity,
                    ppid: fields.get(1).and_then(|v| v.parse().ok()).unwrap_or(0),
                    name,
                    command,
                    user: uid.to_string(),
                    state: fields.first().and_then(|s| s.chars().next()).unwrap_or('?'),
                    read: counter("rchar").ok_or(std::io::ErrorKind::InvalidData)?,
                    write: counter("wchar").ok_or(std::io::ErrorKind::InvalidData)?,
                    disk_read: counter("read_bytes"),
                    disk_write: counter("write_bytes"),
                })
            };
            match read() {
                Ok(r) => rows.push(r),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied += 1,
                Err(_) => {}
            }
        }
    }
    (rows, scanned, denied)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reading(id: u64, bytes: u64) -> Reading {
        Reading {
            pid: 42,
            identity: id,
            ppid: 1,
            name: "reader".into(),
            command: "reader".into(),
            user: "user".into(),
            state: 'S',
            read: bytes,
            write: 0,
            disk_read: Some(bytes / 2),
            disk_write: Some(0),
        }
    }
    #[test]
    fn baselines_deltas_retention_and_pid_reuse() {
        let mut s = ProcessScanner::new(None);
        let now = Instant::now();
        let first = s.sample(vec![reading(1, 100)], 1, 0, now);
        assert_eq!(first.procs[0].read_bps, 0.0);
        let next = s.sample(vec![reading(1, 300)], 1, 0, now + Duration::from_secs(2));
        assert_eq!(next.procs[0].read_bps, 100.0);
        assert_eq!(next.procs[0].read_total, 200);
        assert_eq!(next.procs[0].io.unwrap().read_bytes_bps, 50.0);
        let gone = s.sample(vec![], 0, 0, now + Duration::from_secs(3));
        assert_eq!(gone.procs[0].state, 'X');
        assert_eq!(gone.procs[0].read_total, 200);
        let reused = s.sample(vec![reading(2, 10000)], 1, 0, now + Duration::from_secs(4));
        assert_eq!(reused.procs[0].read_total, 0);
        assert_eq!(reused.procs[0].read_bps, 0.0);
        assert!(
            s.sample(vec![], 0, 0, now + Duration::from_secs(125))
                .procs
                .is_empty()
        );
    }
    #[test]
    fn native_sampler_can_observe_this_process() {
        let mut s = ProcessScanner::new(Some(std::process::id()));
        assert_eq!(s.scan().procs.len(), 1);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(s.scan().procs[0].identity, s.scan().procs[0].identity);
    }

    #[test]
    fn native_file_sampling_tracks_reads_and_handle_closure() {
        use std::io::Read;
        let path = std::env::temp_dir().join(format!("whouse-read-history-{}", std::process::id()));
        std::fs::write(&path, vec![0u8; 16384]).unwrap();
        let canonical = std::fs::canonicalize(&path).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let mut scanner = ProcessScanner::new(Some(std::process::id()));
        let baseline = scanner.scan();
        let observed = baseline.procs[0]
            .files
            .iter()
            .find(|f| f.path == canonical)
            .expect("open file not observed");
        assert_eq!(observed.access, Access::Read);
        assert_eq!(observed.read_bps, 0.0);
        std::thread::sleep(Duration::from_millis(20));
        file.read_exact(&mut [0u8; 4096]).unwrap();
        let next = scanner.scan();
        let observed = next.procs[0]
            .files
            .iter()
            .find(|f| f.path == canonical)
            .unwrap();
        assert_eq!(observed.pos, 4096);
        assert!(observed.read_bps > 0.0);
        drop(file);
        let closed = scanner.scan();
        assert!(closed.procs[0].files_observed);
        assert!(!closed.procs[0].files.iter().any(|f| f.path == canonical));
        std::fs::remove_file(path).unwrap();
    }
}
