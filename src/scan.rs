//! `/proc` sampler.
//!
//! Finds every process that has a file, directory, mapping or working
//! directory under the watched path, and estimates its I/O from the movement of
//! each file descriptor's offset (`/proc/<pid>/fdinfo/<fd>`) between two scans.
//!
//! Limits worth knowing about:
//! * `pread`/`pwrite` and `mmap` I/O never moves a file offset, so those files
//!   show up as open (with zero rate) but their bytes are invisible here.
//! * Opens that start and finish between two scans are missed entirely.
//! * Other users' processes are unreadable unless we run as root.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const O_ACCMODE: u32 = 0o3;
const O_PATH: u32 = 0o10_000_000;
const PF_KTHREAD: u64 = 0x0020_0000;
const DELETED: &[u8] = b" (deleted)";
/// `/proc/<pid>/maps` is by far the most expensive file to read, and mappings
/// of watched files are long-lived, so each process's maps are re-read about
/// this often (staggered across scans) instead of on every scan.
const MAPS_PERIOD: Duration = Duration::from_secs(2);
/// Process-wide traffic below this is not worth flagging as unexplained.
const HINT_MIN_BPS: f64 = 64.0 * 1024.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Read,
    Write,
    ReadWrite,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    File,
    Dir,
    Map,
    Other,
}

#[derive(Clone, Debug)]
pub struct OpenFile {
    /// Every descriptor in this process that refers to the same open file
    /// (dup, dup2, `2>&1`). Empty for memory mappings.
    pub fds: Vec<u32>,
    pub path: PathBuf,
    pub kind: Kind,
    pub access: Access,
    pub pos: u64,
    pub size: u64,
    pub read_bps: f64,
    pub write_bps: f64,
    pub deleted: bool,
    /// The same open file description is also held by another process
    /// (inherited across fork), so the rate is not exclusively this process's.
    pub shared: bool,
}

impl OpenFile {
    pub fn bps(&self) -> f64 {
        self.read_bps + self.write_bps
    }
}

/// Whole-process counters from `/proc/<pid>/io`, not limited to the watched
/// path. `rchar`/`wchar` count every read/write syscall, `*_bytes` count what
/// actually reached the block layer.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcIo {
    pub rchar_bps: f64,
    pub wchar_bps: f64,
    pub read_bytes_bps: f64,
    pub write_bytes_bps: f64,
}

#[derive(Clone, Debug)]
pub struct ProcActivity {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
    pub cmdline: String,
    pub user: String,
    pub state: char,
    /// Working directory, only when it lies inside the watched path.
    pub cwd: Option<PathBuf>,
    pub files: Vec<OpenFile>,
    pub read_bps: f64,
    pub write_bps: f64,
    /// Bytes attributed to the path since whouse started.
    pub read_total: u64,
    pub write_total: u64,
    pub io: Option<ProcIo>,
    /// Upper bounds for traffic the offsets cannot explain: the process moves
    /// this much overall, holds files under the path that it could be using,
    /// yet no offset there advanced. That is what `pread`, `mmap` and other
    /// positional I/O look like, but it may just as well be other files or
    /// sockets, hence "up to". Zero when there is nothing to flag.
    pub hint_read_bps: f64,
    pub hint_write_bps: f64,
    /// Folder the process is currently busiest in.
    pub focus: Option<PathBuf>,
    /// `Some` once the process stopped touching the path; kept on screen for a
    /// few seconds so rows don't flicker in and out.
    pub idle_for: Option<Duration>,
}

impl ProcActivity {
    pub fn bps(&self) -> f64 {
        self.read_bps + self.write_bps
    }

    pub fn hint_bps(&self) -> f64 {
        self.hint_read_bps + self.hint_write_bps
    }

    pub fn count(&self, kind: Kind) -> usize {
        self.files.iter().filter(|f| f.kind == kind).count()
    }
}

#[derive(Clone, Debug, Default)]
pub struct FolderActivity {
    pub path: PathBuf,
    pub read_bps: f64,
    pub write_bps: f64,
    pub pids: Vec<u32>,
    /// Files and mappings held open directly inside this folder.
    pub files: usize,
    /// Handles on the folder itself (someone is listing it).
    pub opened: usize,
}

impl FolderActivity {
    pub fn bps(&self) -> f64 {
        self.read_bps + self.write_bps
    }
}

#[derive(Clone, Debug, Default)]
pub struct Totals {
    pub read_bps: f64,
    pub write_bps: f64,
    pub read_total: u64,
    pub write_total: u64,
    pub procs: usize,
    pub files: usize,
    pub dirs: usize,
    pub maps: usize,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub procs: Vec<ProcActivity>,
    pub folders: Vec<FolderActivity>,
    pub totals: Totals,
    /// Processes inspected, and how many of those we were not allowed to read.
    pub scanned: usize,
    pub denied: usize,
    pub scan_time: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub maps: bool,
    pub linger: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            maps: true,
            linger: Duration::from_secs(4),
        }
    }
}

pub enum Cmd {
    Interval(Duration),
    Refresh,
}

// ---------------------------------------------------------------------------

/// (device, inode, open flags, offset): descriptors that agree on all four are
/// treated as one open file description.
type ShareKey = (u64, u64, u32, u64);

struct RawFd {
    fd: u32,
    path: PathBuf,
    deleted: bool,
    kind: Kind,
    access: Access,
    dev: u64,
    ino: u64,
    flags: u32,
    pos: u64,
    size: u64,
}

impl RawFd {
    fn key(&self) -> ShareKey {
        (self.dev, self.ino, self.flags, self.pos)
    }
}

#[derive(Clone)]
struct RawMap {
    path: PathBuf,
    deleted: bool,
    size: u64,
}

struct RawProc {
    pid: u32,
    fds: Vec<RawFd>,
    maps: Vec<RawMap>,
    cwd: Option<PathBuf>,
}

enum PidScan {
    Hit(RawProc),
    Miss,
    Denied,
    Gone,
}

struct FdMem {
    dev: u64,
    ino: u64,
    pos: u64,
}

#[derive(Clone, Copy)]
struct IoCounters {
    rchar: u64,
    wchar: u64,
    read_bytes: u64,
    write_bytes: u64,
}

#[derive(Default)]
struct ProcMem {
    starttime: u64,
    cmdline: String,
    fds: HashMap<u32, FdMem>,
    io: Option<IoCounters>,
    read_total: u64,
    write_total: u64,
    last_seen: Option<Instant>,
    last_row: Option<ProcActivity>,
}

#[derive(Default)]
struct FolderAcc {
    read_bps: f64,
    write_bps: f64,
    pids: HashSet<u32>,
    files: usize,
    opened: usize,
}

pub struct Scanner {
    root: PathBuf,
    opts: Options,
    own_pid: u32,
    users: HashMap<u32, String>,
    memory: HashMap<u32, ProcMem>,
    prev_time: Option<Instant>,
    session_read: u64,
    session_write: u64,
    tick: u64,
    /// Last maps result per pid and the scan it was last used in.
    maps_cache: HashMap<u32, (u64, Vec<RawMap>)>,
    /// Processes we may not inspect, and whether each is a kernel thread
    /// (which is not worth reporting as "unreadable").
    denied: HashMap<u32, bool>,
}

impl Scanner {
    pub fn new(root: PathBuf, opts: Options) -> Self {
        Self {
            root,
            opts,
            own_pid: std::process::id(),
            users: HashMap::new(),
            memory: HashMap::new(),
            prev_time: None,
            session_read: 0,
            session_write: 0,
            tick: 0,
            maps_cache: HashMap::new(),
            denied: HashMap::new(),
        }
    }

    pub fn scan(&mut self) -> Snapshot {
        let t0 = Instant::now();
        let dt = self.prev_time.map(|p| t0.duration_since(p));
        let baseline = dt.is_none();
        let secs = dt.map_or(1.0, |d| d.as_secs_f64().max(0.001));
        self.prev_time = Some(t0);
        self.tick += 1;
        let slots = dt.map_or(1, |d| {
            (MAPS_PERIOD.as_millis() / d.as_millis().max(1)).clamp(1, 16) as u64
        });
        let mut denied_now: HashMap<u32, bool> = HashMap::new();

        // 1. Walk /proc and collect everything that touches the path.
        let mut raws = Vec::new();
        let (mut scanned, mut denied) = (0usize, 0usize);
        if let Ok(rd) = fs::read_dir("/proc") {
            for ent in rd.flatten() {
                let Some(pid) = ent.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                    continue;
                };
                if pid == self.own_pid {
                    continue;
                }
                let maps_due = (self.tick + pid as u64).is_multiple_of(slots);
                match self.scan_pid(pid, maps_due) {
                    PidScan::Hit(raw) => {
                        scanned += 1;
                        raws.push(raw);
                    }
                    PidScan::Miss => scanned += 1,
                    PidScan::Denied => {
                        let kthread = self
                            .denied
                            .get(&pid)
                            .copied()
                            .unwrap_or_else(|| is_kernel_thread(pid));
                        denied_now.insert(pid, kthread);
                        if !kthread {
                            scanned += 1;
                            denied += 1;
                        }
                    }
                    PidScan::Gone => {}
                }
            }
        }

        self.denied = denied_now;
        let tick = self.tick;
        self.maps_cache
            .retain(|_, (last_used, _)| *last_used == tick);

        // 2. Which open file descriptions are held by more than one process?
        let mut holders: HashMap<ShareKey, (u32, bool)> = HashMap::new();
        for raw in &raws {
            for fd in raw.fds.iter().filter(|f| f.kind == Kind::File) {
                let e = holders.entry(fd.key()).or_insert((raw.pid, false));
                if e.0 != raw.pid {
                    e.1 = true;
                }
            }
        }

        // 3. Turn raw descriptors into per-process rows and per-folder totals.
        let mut rows = Vec::with_capacity(raws.len());
        let mut folders: HashMap<PathBuf, FolderAcc> = HashMap::new();
        let mut counted: HashSet<ShareKey> = HashSet::new();
        let mut totals = Totals::default();
        let mut live = HashSet::new();
        let (mut inc_read, mut inc_write) = (0u64, 0u64);

        for raw in raws {
            let Some(stat) = read_stat(raw.pid) else {
                continue;
            };
            let mem = self.memory.entry(raw.pid).or_default();
            if mem.starttime != stat.starttime {
                *mem = ProcMem {
                    starttime: stat.starttime,
                    ..ProcMem::default()
                };
            }
            if mem.cmdline.is_empty() {
                mem.cmdline = read_cmdline(raw.pid, &stat.comm);
            }
            let uid = fs::metadata(format!("/proc/{}", raw.pid)).map_or(0, |m| m.uid());
            let user = self
                .users
                .entry(uid)
                .or_insert_with(|| user_name(uid))
                .clone();

            // Process-wide counters, also used to split read/write for O_RDWR.
            let io_now = read_io(raw.pid);
            let (mut proc_io, mut d_rchar, mut d_wchar) = (None, 0u64, 0u64);
            if let (Some(a), Some(b)) = (mem.io, io_now) {
                d_rchar = b.rchar.saturating_sub(a.rchar);
                d_wchar = b.wchar.saturating_sub(a.wchar);
                proc_io = Some(ProcIo {
                    rchar_bps: d_rchar as f64 / secs,
                    wchar_bps: d_wchar as f64 / secs,
                    read_bytes_bps: b.read_bytes.saturating_sub(a.read_bytes) as f64 / secs,
                    write_bytes_bps: b.write_bytes.saturating_sub(a.write_bytes) as f64 / secs,
                });
            }
            let read_frac = if d_rchar + d_wchar > 0 {
                d_rchar as f64 / (d_rchar + d_wchar) as f64
            } else {
                0.5
            };

            let mut files: Vec<OpenFile> = Vec::new();
            let mut groups: HashMap<ShareKey, usize> = HashMap::new();
            let mut next_fds = HashMap::new();
            let (mut p_read, mut p_write) = (0f64, 0f64);

            for fd in &raw.fds {
                next_fds.insert(
                    fd.fd,
                    FdMem {
                        dev: fd.dev,
                        ino: fd.ino,
                        pos: fd.pos,
                    },
                );
                let key = fd.key();
                if let Some(&i) = groups.get(&key) {
                    files[i].fds.push(fd.fd);
                    continue;
                }
                let shared = fd.kind == Kind::File && holders.get(&key).is_some_and(|h| h.1);

                let delta = if fd.kind != Kind::File {
                    0
                } else {
                    match mem.fds.get(&fd.fd) {
                        Some(prev) if prev.dev == fd.dev && prev.ino == fd.ino => {
                            fd.pos.saturating_sub(prev.pos)
                        }
                        // Not there at the previous scan, so it was opened
                        // since: everything before the offset happened in this
                        // interval. Inherited descriptors would be misleading.
                        _ if baseline || shared => 0,
                        _ if fd.size > 0 => fd.pos.min(fd.size),
                        _ => fd.pos,
                    }
                } as f64;
                let (r, w) = match fd.access {
                    Access::Read => (delta, 0.0),
                    Access::Write => (0.0, delta),
                    Access::ReadWrite => (delta * read_frac, delta * (1.0 - read_frac)),
                };
                let (read_bps, write_bps) = (r / secs, w / secs);
                p_read += read_bps;
                p_write += write_bps;
                mem.read_total += r as u64;
                mem.write_total += w as u64;

                // Global figures count each open file description once.
                let first = fd.kind != Kind::File || counted.insert(key);
                if first {
                    totals.read_bps += read_bps;
                    totals.write_bps += write_bps;
                    inc_read += r as u64;
                    inc_write += w as u64;
                }
                let folder = match fd.kind {
                    Kind::Dir => fd.path.clone(),
                    _ => parent_or(&fd.path, &self.root),
                };
                let acc = folders.entry(folder).or_default();
                acc.pids.insert(raw.pid);
                if fd.kind == Kind::Dir {
                    acc.opened += 1;
                } else {
                    acc.files += 1;
                }
                if first {
                    acc.read_bps += read_bps;
                    acc.write_bps += write_bps;
                }

                groups.insert(key, files.len());
                files.push(OpenFile {
                    fds: vec![fd.fd],
                    path: fd.path.clone(),
                    kind: fd.kind,
                    access: fd.access,
                    pos: fd.pos,
                    size: fd.size,
                    read_bps,
                    write_bps,
                    deleted: fd.deleted,
                    shared,
                });
            }

            for m in &raw.maps {
                let acc = folders.entry(parent_or(&m.path, &self.root)).or_default();
                acc.pids.insert(raw.pid);
                acc.files += 1;
                files.push(OpenFile {
                    fds: Vec::new(),
                    path: m.path.clone(),
                    kind: Kind::Map,
                    access: Access::Read,
                    pos: 0,
                    size: m.size,
                    read_bps: 0.0,
                    write_bps: 0.0,
                    deleted: m.deleted,
                    shared: false,
                });
            }
            if let Some(cwd) = &raw.cwd {
                folders.entry(cwd.clone()).or_default().pids.insert(raw.pid);
            }

            files.sort_by(|a, b| {
                b.bps()
                    .total_cmp(&a.bps())
                    .then(kind_rank(a.kind).cmp(&kind_rank(b.kind)))
                    .then_with(|| a.path.cmp(&b.path))
            });
            totals.files += files.iter().filter(|f| f.kind == Kind::File).count();
            totals.dirs += files.iter().filter(|f| f.kind == Kind::Dir).count();
            totals.maps += files.iter().filter(|f| f.kind == Kind::Map).count();
            totals.procs += 1;

            // Could this process be reading / writing the watched files at all?
            let can_read = files
                .iter()
                .any(|f| matches!(f.kind, Kind::File | Kind::Map) && f.access != Access::Write);
            let can_write = files
                .iter()
                .any(|f| f.kind == Kind::File && f.access != Access::Read);
            let unexplained = |total: f64, attributed: f64, possible: bool| {
                let rest = total - attributed;
                if possible && rest >= HINT_MIN_BPS {
                    rest
                } else {
                    0.0
                }
            };
            let (hint_read_bps, hint_write_bps) = proc_io.map_or((0.0, 0.0), |io| {
                (
                    unexplained(io.rchar_bps.max(io.read_bytes_bps), p_read, can_read),
                    unexplained(io.wchar_bps.max(io.write_bytes_bps), p_write, can_write),
                )
            });

            let row = ProcActivity {
                pid: raw.pid,
                ppid: stat.ppid,
                comm: stat.comm,
                cmdline: mem.cmdline.clone(),
                user,
                state: stat.state,
                focus: focus_of(&files, raw.cwd.as_ref()),
                cwd: raw.cwd,
                files,
                read_bps: p_read,
                write_bps: p_write,
                read_total: mem.read_total,
                write_total: mem.write_total,
                io: proc_io,
                hint_read_bps,
                hint_write_bps,
                idle_for: None,
            };
            mem.fds = next_fds;
            mem.io = io_now;
            mem.last_seen = Some(t0);
            mem.last_row = Some(ProcActivity {
                files: Vec::new(),
                read_bps: 0.0,
                write_bps: 0.0,
                hint_read_bps: 0.0,
                hint_write_bps: 0.0,
                io: None,
                ..row.clone()
            });
            live.insert(raw.pid);
            rows.push(row);
        }

        // 4. Keep recently active processes around, drop stale memory.
        let linger = self.opts.linger;
        for (pid, mem) in &self.memory {
            if live.contains(pid) {
                continue;
            }
            if let (Some(seen), Some(row)) = (mem.last_seen, &mem.last_row) {
                let idle = t0.duration_since(seen);
                if idle < linger {
                    rows.push(ProcActivity {
                        idle_for: Some(idle),
                        ..row.clone()
                    });
                }
            }
        }
        self.memory.retain(|pid, mem| {
            live.contains(pid) || mem.last_seen.is_some_and(|s| t0.duration_since(s) < linger)
        });

        rows.sort_by(|a, b| {
            b.bps()
                .total_cmp(&a.bps())
                .then(b.hint_bps().total_cmp(&a.hint_bps()))
                .then(a.pid.cmp(&b.pid))
        });

        let mut folders: Vec<FolderActivity> = folders
            .into_iter()
            .map(|(path, acc)| {
                let mut pids: Vec<u32> = acc.pids.into_iter().collect();
                pids.sort_unstable();
                FolderActivity {
                    path,
                    read_bps: acc.read_bps,
                    write_bps: acc.write_bps,
                    pids,
                    files: acc.files,
                    opened: acc.opened,
                }
            })
            .collect();
        sort_folders(&mut folders);

        self.session_read += inc_read;
        self.session_write += inc_write;
        totals.read_total = self.session_read;
        totals.write_total = self.session_write;

        Snapshot {
            procs: rows,
            folders,
            totals,
            scanned,
            denied,
            scan_time: t0.elapsed(),
        }
    }

    fn scan_pid(&mut self, pid: u32, maps_due: bool) -> PidScan {
        let mut fds = Vec::new();
        let walked = for_each_fd(pid, |fd, target| {
            let Some((path, deleted)) = under_root(&self.root, target) else {
                return;
            };
            let link_path = format!("/proc/{pid}/fd/{fd}");
            let Ok(meta) = fs::metadata(&link_path) else {
                return;
            };
            let info = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")).unwrap_or_default();
            let (pos, flags) = parse_fdinfo(&info);

            let ft = meta.file_type();
            let kind = if ft.is_dir() {
                Kind::Dir
            } else if flags & O_PATH != 0 {
                Kind::Other
            } else if ft.is_file() || ft.is_block_device() {
                Kind::File
            } else {
                Kind::Other
            };
            fds.push(RawFd {
                fd,
                path,
                deleted,
                kind,
                access: match flags & O_ACCMODE {
                    0 => Access::Read,
                    1 => Access::Write,
                    _ => Access::ReadWrite,
                },
                dev: meta.dev(),
                ino: meta.ino(),
                flags,
                pos,
                size: meta.len(),
            });
        });
        if let Err(e) = walked {
            return if e.kind() == io::ErrorKind::PermissionDenied {
                PidScan::Denied
            } else {
                PidScan::Gone
            };
        }

        let cwd = fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .and_then(|l| under_root(&self.root, l.as_os_str().as_bytes()))
            .map(|(p, _)| p);
        let maps = if self.opts.maps {
            let entry = self
                .maps_cache
                .entry(pid)
                .or_insert_with(|| (0, Vec::new()));
            if maps_due || entry.0 == 0 {
                entry.1 = scan_maps(&self.root, pid);
            }
            entry.0 = self.tick;
            entry
                .1
                .iter()
                .filter(|m| !fds.iter().any(|f| f.path.as_os_str() == m.path.as_os_str()))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };

        if fds.is_empty() && maps.is_empty() && cwd.is_none() {
            PidScan::Miss
        } else {
            PidScan::Hit(RawProc {
                pid,
                fds,
                maps,
                cwd,
            })
        }
    }
}

/// Mapped files of `pid` that live under `root`.
fn scan_maps(root: &Path, pid: u32) -> Vec<RawMap> {
    let Ok(data) = fs::read(format!("/proc/{pid}/maps")) else {
        return Vec::new();
    };
    let mut out: Vec<RawMap> = Vec::new();
    // A file usually shows up as several adjacent mappings (text, data, ...).
    let mut previous: &[u8] = &[];
    for line in data.split(|&b| b == b'\n') {
        let Some(raw_path) = map_path(line) else {
            continue;
        };
        if raw_path == previous {
            continue;
        }
        previous = raw_path;
        let Some((path, deleted)) = under_root(root, raw_path) else {
            continue;
        };
        let size = fs::metadata(&path).map_or(0, |m| m.len());
        out.push(RawMap {
            path,
            deleted,
            size,
        });
    }
    out.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
    out.dedup_by(|a, b| a.path.as_os_str() == b.path.as_os_str());
    out
}

/// Returns the path (and whether it is flagged deleted) if the link target
/// `bytes` lies inside `root`. Cheap for the common "no" answer: no allocation.
fn under_root(root: &Path, bytes: &[u8]) -> Option<(PathBuf, bool)> {
    if !bytes.starts_with(root.as_os_str().as_bytes()) {
        return None;
    }
    let (bytes, deleted) = match bytes.strip_suffix(DELETED) {
        Some(b) => (b, true),
        None => (bytes, false),
    };
    let path = Path::new(OsStr::from_bytes(bytes));
    path.starts_with(root)
        .then(|| (path.to_path_buf(), deleted))
}

struct Dir(*mut libc::DIR);

impl Drop for Dir {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful `opendir` and is closed once.
        unsafe { libc::closedir(self.0) };
    }
}

/// Calls `f(fd, link_target)` for every descriptor of `pid`.
///
/// This is the hot loop of a scan (thousands of descriptors across the system),
/// so it uses `readlinkat` into a stack buffer instead of `fs::read_link`, which
/// allocates, grows and shrinks a `PathBuf` for every single descriptor.
fn for_each_fd(pid: u32, mut f: impl FnMut(u32, &[u8])) -> io::Result<()> {
    let dir = CString::new(format!("/proc/{pid}/fd")).map_err(io::Error::other)?;
    // SAFETY: `dir` is a valid C string; `Dir` closes the stream on every exit path.
    let stream = unsafe { libc::opendir(dir.as_ptr()) };
    if stream.is_null() {
        return Err(io::Error::last_os_error());
    }
    let dir = Dir(stream);
    // SAFETY: `dir.0` stays valid until `dir` drops.
    let dir_fd = unsafe { libc::dirfd(dir.0) };
    let mut buf = [0u8; libc::PATH_MAX as usize];
    loop {
        // SAFETY: valid stream; the returned entry is only used before the next
        // `readdir` call.
        let ent = unsafe { libc::readdir(dir.0) };
        if ent.is_null() {
            return Ok(());
        }
        // SAFETY: `d_name` is a NUL-terminated name inside the entry.
        let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        let Some(fd) = std::str::from_utf8(name.to_bytes())
            .ok()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue; // "." and ".."
        };
        // SAFETY: `buf` is writable for `buf.len()` bytes and `name` is a valid C string.
        let n =
            unsafe { libc::readlinkat(dir_fd, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            f(fd, &buf[..n as usize]);
        }
    }
}

/// Busiest first, then the most crowded, then by name.
pub fn sort_folders(folders: &mut [FolderActivity]) {
    folders.sort_by(|a, b| {
        b.bps()
            .total_cmp(&a.bps())
            .then(b.pids.len().cmp(&a.pids.len()))
            .then_with(|| a.path.cmp(&b.path))
    });
}

fn kind_rank(k: Kind) -> u8 {
    match k {
        Kind::File => 0,
        Kind::Map => 1,
        Kind::Dir => 2,
        Kind::Other => 3,
    }
}

fn parent_or(path: &Path, fallback: &Path) -> PathBuf {
    path.parent().unwrap_or(fallback).to_path_buf()
}

/// Where a process is busiest: the folder of its hottest file, else a folder it
/// has open, else its working directory.
fn focus_of(files: &[OpenFile], cwd: Option<&PathBuf>) -> Option<PathBuf> {
    // `files` is sorted hottest first, regular files before everything else.
    if let Some(f) = files.iter().find(|f| f.kind == Kind::File) {
        return f.path.parent().map(Path::to_path_buf);
    }
    if let Some(d) = files.iter().find(|f| f.kind == Kind::Dir) {
        return Some(d.path.clone());
    }
    if let Some(f) = files.first() {
        return f.path.parent().map(Path::to_path_buf);
    }
    cwd.cloned()
}

/// The path column of a `/proc/<pid>/maps` line (`addr perms off dev inode path`).
fn map_path(line: &[u8]) -> Option<&[u8]> {
    let mut rest = line;
    for _ in 0..5 {
        let i = rest.iter().position(|&b| b == b' ')?;
        rest = &rest[i + 1..];
        while rest.first() == Some(&b' ') {
            rest = &rest[1..];
        }
    }
    rest.starts_with(b"/").then_some(rest)
}

fn parse_fdinfo(s: &str) -> (u64, u32) {
    let (mut pos, mut flags) = (0, 0);
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("pos:") {
            pos = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("flags:") {
            flags = u32::from_str_radix(v.trim(), 8).unwrap_or(0);
        }
    }
    (pos, flags)
}

struct Stat {
    comm: String,
    state: char,
    ppid: u32,
    starttime: u64,
    flags: u64,
}

fn read_stat(pid: u32) -> Option<Stat> {
    let raw = fs::read(format!("/proc/{pid}/stat")).ok()?;
    let s = String::from_utf8_lossy(&raw);
    // The command name is parenthesised and may itself contain ')' or spaces.
    let (open, close) = (s.find('(')?, s.rfind(')')?);
    let rest: Vec<&str> = s.get(close + 1..)?.split_whitespace().collect();
    Some(Stat {
        comm: s[open + 1..close].to_string(),
        state: rest.first()?.chars().next()?,
        ppid: rest.get(1)?.parse().ok()?,
        flags: rest.get(6)?.parse().ok()?,
        starttime: rest.get(19)?.parse().ok()?,
    })
}

fn is_kernel_thread(pid: u32) -> bool {
    read_stat(pid).is_some_and(|s| s.flags & PF_KTHREAD != 0)
}

fn read_cmdline(pid: u32, comm: &str) -> String {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let joined = raw
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a))
        .collect::<Vec<_>>()
        .join(" ");
    if joined.is_empty() {
        format!("[{comm}]")
    } else {
        joined
    }
}

fn read_io(pid: u32) -> Option<IoCounters> {
    let s = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let mut io = IoCounters {
        rchar: 0,
        wchar: 0,
        read_bytes: 0,
        write_bytes: 0,
    };
    for line in s.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim().parse().unwrap_or(0);
        match k {
            "rchar" => io.rchar = v,
            "wchar" => io.wchar = v,
            "read_bytes" => io.read_bytes = v,
            "write_bytes" => io.write_bytes = v,
            _ => {}
        }
    }
    Some(io)
}

fn user_name(uid: u32) -> String {
    let mut buf = vec![0u8; 1024];
    // SAFETY: `pwd` and `res` outlive the call and `buf` is the scratch space
    // getpwuid_r is told about; `pw_name` points into `buf` while we read it.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut res: *mut libc::passwd = std::ptr::null_mut();
        loop {
            let rc = libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr().cast(), buf.len(), &mut res);
            if rc == libc::ERANGE && buf.len() < 1 << 16 {
                buf.resize(buf.len() * 2, 0);
                continue;
            }
            if rc != 0 || res.is_null() {
                return uid.to_string();
            }
            return CStr::from_ptr(pwd.pw_name).to_string_lossy().into_owned();
        }
    }
}

/// Runs the scanner on its own thread until `emit` returns false or the command
/// channel is dropped. The first follow-up scan comes quickly so the UI gets
/// real rates almost immediately.
pub fn spawn(
    mut scanner: Scanner,
    mut interval: Duration,
    cmds: Receiver<Cmd>,
    mut emit: impl FnMut(Snapshot) -> bool + Send + 'static,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut first = true;
        loop {
            let started = Instant::now();
            if !emit(scanner.scan()) {
                return;
            }
            let wait = if first {
                interval.min(Duration::from_millis(250))
            } else {
                interval
            };
            first = false;
            let mut deadline = started + wait;
            loop {
                match cmds.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Err(RecvTimeoutError::Timeout) | Ok(Cmd::Refresh) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                    Ok(Cmd::Interval(d)) => {
                        interval = d;
                        deadline = started + d;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::process::Command;

    /// Scratch directory under the system temp dir, canonicalised because the
    /// scanner compares against `/proc` link targets.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("whouse-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(dir).unwrap()
    }

    /// A scanner that does not skip this test process.
    fn scanner(root: &Path) -> Scanner {
        let mut s = Scanner::new(root.to_path_buf(), Options::default());
        s.own_pid = u32::MAX;
        s
    }

    fn me(snap: &Snapshot) -> &ProcActivity {
        snap.procs
            .iter()
            .find(|p| p.pid == std::process::id())
            .expect("test process not reported")
    }

    #[test]
    fn reports_read_rate_for_open_file_and_dir_handle() {
        let dir = scratch("rate");
        let file = dir.join("a.bin");
        fs::write(&file, vec![7u8; 8 << 20]).unwrap();
        let mut sc = scanner(&dir);

        let mut f = fs::File::open(&file).unwrap();
        let _dirfd = fs::File::open(&dir).unwrap();
        let base = sc.scan();
        assert_eq!(me(&base).read_bps, 0.0, "first scan is only a baseline");

        let mut buf = vec![0u8; 4 << 20];
        f.read_exact(&mut buf).unwrap();
        thread::sleep(Duration::from_millis(50));
        let snap = sc.scan();
        let p = me(&snap);

        let of = p
            .files
            .iter()
            .find(|x| x.path == file)
            .expect("file missing");
        assert_eq!(of.kind, Kind::File);
        assert_eq!(of.access, Access::Read);
        assert_eq!(of.pos, 4 << 20);
        assert_eq!(of.size, 8 << 20);
        assert!(of.read_bps > 0.0 && of.write_bps == 0.0);
        assert_eq!(p.read_total, 4 << 20);
        assert_eq!(snap.totals.read_total, 4 << 20);
        assert_eq!(p.focus.as_deref(), Some(dir.as_path()));
        let d = p
            .files
            .iter()
            .find(|x| x.kind == Kind::Dir)
            .expect("dir handle missing");
        assert_eq!(d.path, dir);
        assert_eq!(snap.folders[0].path, dir);

        // Nothing moved since: rate drops back to zero, totals stay.
        let snap = sc.scan();
        assert_eq!(me(&snap).read_bps, 0.0);
        assert_eq!(me(&snap).read_total, 4 << 20);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn counts_bytes_of_a_file_opened_between_scans() {
        let dir = scratch("newfd");
        let file = dir.join("b.bin");
        fs::write(&file, vec![1u8; 2 << 20]).unwrap();
        let mut sc = scanner(&dir);
        sc.scan();

        let mut f = fs::File::open(&file).unwrap();
        let mut buf = vec![0u8; 1 << 20];
        f.read_exact(&mut buf).unwrap();
        let snap = sc.scan();
        assert_eq!(me(&snap).read_total, 1 << 20);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_only_files_count_as_writes_and_dups_are_merged() {
        let dir = scratch("write");
        let mut sc = scanner(&dir);
        let mut f = fs::File::create(dir.join("w.bin")).unwrap();
        let dup = f.try_clone().unwrap();
        sc.scan();

        f.write_all(&vec![0u8; 1 << 20]).unwrap();
        let snap = sc.scan();
        let p = me(&snap);
        assert_eq!(p.files.iter().filter(|x| x.kind == Kind::File).count(), 1);
        let of = &p.files[0];
        assert_eq!(of.fds.len(), 2, "dup'd descriptor should be merged: {of:?}");
        assert_eq!(of.access, Access::Write);
        assert_eq!(p.write_total, 1 << 20, "dup must not double count");
        assert_eq!(snap.totals.write_total, 1 << 20);
        drop(dup);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flags_deleted_files() {
        let dir = scratch("deleted");
        let file = dir.join("gone.bin");
        fs::write(&file, b"x").unwrap();
        let _f = fs::File::open(&file).unwrap();
        fs::remove_file(&file).unwrap();
        let snap = scanner(&dir).scan();
        let of = me(&snap)
            .files
            .iter()
            .find(|x| x.path == file)
            .expect("deleted file missing");
        assert!(of.deleted);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sees_memory_mappings_without_an_fd() {
        let dir = scratch("mmap");
        let file = dir.join("m.bin");
        fs::write(&file, vec![0u8; 8192]).unwrap();
        let f = fs::File::open(&file).unwrap();
        let len = 8192;
        // SAFETY: plain read-only private mapping of a file we just created;
        // unmapped again below.
        let addr = unsafe {
            use std::os::fd::AsRawFd;
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                f.as_raw_fd(),
                0,
            )
        };
        assert_ne!(addr, libc::MAP_FAILED);
        drop(f);

        let snap = scanner(&dir).scan();
        // SAFETY: `addr`/`len` come from the mmap above.
        unsafe { libc::munmap(addr, len) };
        let of = me(&snap)
            .files
            .iter()
            .find(|x| x.path == file)
            .expect("mapping missing");
        assert_eq!(of.kind, Kind::Map);
        assert!(of.fds.is_empty());
        assert_eq!(of.size, 8192);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sees_other_processes_by_working_directory_and_lingers_after_exit() {
        let dir = scratch("cwd");
        let mut child = Command::new("sleep")
            .arg("30")
            .current_dir(&dir)
            .spawn()
            .unwrap();
        let mut sc = scanner(&dir);
        sc.opts.linger = Duration::from_millis(400);

        let snap = sc.scan();
        let p = snap
            .procs
            .iter()
            .find(|p| p.pid == child.id())
            .expect("child missing");
        assert_eq!(p.cwd.as_deref(), Some(dir.as_path()));
        assert_eq!(p.comm, "sleep");
        assert_eq!(p.cmdline, "sleep 30");
        assert_eq!(p.focus.as_deref(), Some(dir.as_path()));
        assert!(p.idle_for.is_none());

        child.kill().unwrap();
        child.wait().unwrap();
        let snap = sc.scan();
        let p = snap
            .procs
            .iter()
            .find(|p| p.pid == child.id())
            .expect("row should linger");
        assert!(p.idle_for.is_some());
        thread::sleep(Duration::from_millis(500));
        let snap = sc.scan();
        assert!(
            snap.procs.iter().all(|p| p.pid != child.id()),
            "row should expire"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flags_traffic_the_offsets_cannot_explain() {
        use std::os::unix::fs::FileExt;
        let dir = scratch("pread");
        let file = dir.join("db.bin");
        fs::write(&file, vec![3u8; 8 << 20]).unwrap();
        let f = fs::File::open(&file).unwrap();
        let mut sc = scanner(&dir);
        sc.scan();

        let mut buf = vec![0u8; 1 << 20];
        for _ in 0..8 {
            f.read_at(&mut buf, 0).unwrap(); // positional: the offset stays at 0
        }
        thread::sleep(Duration::from_millis(50));
        let snap = sc.scan();
        let p = me(&snap);
        assert_eq!(p.files[0].pos, 0);
        assert_eq!(p.read_bps, 0.0, "nothing is attributable");
        assert!(
            p.hint_read_bps > 1e6,
            "8 MiB of pread should show as an upper bound: {}",
            p.hint_read_bps
        );
        assert_eq!(
            p.hint_write_bps, 0.0,
            "a read-only file cannot explain writes"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_hint_when_the_process_holds_nothing_it_could_be_reading() {
        let dir = scratch("nohint");
        let mut sc = scanner(&dir);
        let _dirfd = fs::File::open(&dir).unwrap(); // only a directory handle
        sc.scan();
        fs::write(
            std::env::temp_dir().join(format!("whouse-noise-{}", std::process::id())),
            vec![0u8; 4 << 20],
        )
        .unwrap();
        let snap = sc.scan();
        assert_eq!(me(&snap).hint_read_bps + me(&snap).hint_write_bps, 0.0);
        let _ = fs::remove_file(
            std::env::temp_dir().join(format!("whouse-noise-{}", std::process::id())),
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn can_watch_a_single_file() {
        let dir = scratch("single");
        let file = dir.join("only.bin");
        fs::write(&file, vec![0u8; 1 << 20]).unwrap();
        let other = dir.join("other.bin");
        fs::write(&other, b"x").unwrap();
        let mut sc = scanner(&file);
        let mut f = fs::File::open(&file).unwrap();
        let _o = fs::File::open(&other).unwrap();
        sc.scan();
        f.read_exact(&mut vec![0u8; 512 << 10]).unwrap();
        let snap = sc.scan();
        let p = me(&snap);
        assert_eq!(p.files.len(), 1, "only the watched file: {:?}", p.files);
        assert_eq!(p.files[0].path, file);
        assert_eq!(p.read_total, 512 << 10);
        assert_eq!(p.focus.as_deref(), Some(dir.as_path()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ignores_sibling_directories_with_a_common_prefix() {
        let base = scratch("prefix");
        let (watched, sibling) = (base.join("music"), base.join("music2"));
        fs::create_dir_all(&watched).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let _f = fs::File::create(sibling.join("x")).unwrap();
        let snap = scanner(&watched).scan();
        assert!(snap.procs.iter().all(|p| p.pid != std::process::id()));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn parses_proc_formats() {
        assert_eq!(
            parse_fdinfo("pos:\t1234\nflags:\t0100002\nmnt_id:\t26\n"),
            (1234, 0o100002)
        );
        let line = b"7f00-7f01 r--p 00000000 103:02 12345      /home/a b/c.so (deleted)";
        assert_eq!(map_path(line), Some(&b"/home/a b/c.so (deleted)"[..]));
        assert_eq!(
            map_path(b"7f00-7f01 rw-p 00000000 00:00 0                 [heap]"),
            None
        );
        assert_eq!(map_path(b"7f00-7f01 rw-p 00000000 00:00 0 "), None);
    }

    /// `cargo test --release bench_scan -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_scan() {
        for (root, maps) in [
            ("/home", false),
            ("/home", true),
            (env!("CARGO_MANIFEST_DIR"), false),
            (env!("CARGO_MANIFEST_DIR"), true),
        ] {
            let mut sc = Scanner::new(
                PathBuf::from(root),
                Options {
                    maps,
                    ..Options::default()
                },
            );
            sc.scan();
            let n = 40;
            let t = Instant::now();
            let mut last = None;
            for _ in 0..n {
                thread::sleep(Duration::from_millis(20));
                last = Some(sc.scan());
            }
            let busy = t.elapsed() - Duration::from_millis(20 * n);
            let last = last.unwrap();
            println!(
                "{root:<30} maps={maps:<5} avg {:>5.1} ms/scan  ({} procs inspected, {} denied, {} with hits)",
                busy.as_secs_f64() * 1000.0 / n as f64,
                last.scanned,
                last.denied,
                last.procs.len()
            );
        }
    }
}
