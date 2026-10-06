//! Shared activity models and platform dispatch.
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
#[cfg(target_os = "linux")]
#[path = "scan_linux.rs"]
mod platform;
#[cfg(windows)]
#[path = "scan_windows.rs"]
mod platform;
#[path = "process_io.rs"]
pub mod process_io;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    #[default]
    Path,
    ReadHistory,
}

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
    pub identity: u64,
    pub ppid: u32,
    pub comm: String,
    pub cmdline: String,
    pub user: String,
    pub state: char,
    /// Working directory, only when it lies inside the watched path.
    pub cwd: Option<PathBuf>,
    pub files: Vec<OpenFile>,
    /// Whether file handles were successfully sampled for this process.
    pub files_observed: bool,
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
    pub mode: Mode,
    pub warning: Option<String>,
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
    #[cfg_attr(windows, allow(dead_code))] // Windows does not expose /proc-style mappings.
    pub maps: bool,
    pub mode: Mode,
    pub pid: Option<u32>,
    pub linger: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            maps: true,
            mode: Mode::Path,
            pid: None,
            linger: Duration::from_secs(4),
        }
    }
}

pub enum Cmd {
    Interval(Duration),
    Refresh,
    Mode(Mode),
}

pub struct Scanner {
    root: PathBuf,
    opts: Options,
    path: platform::PathScanner,
    processes: process_io::ProcessScanner,
    mode: Mode,
}
impl Scanner {
    pub fn new(root: PathBuf, opts: Options) -> Self {
        Self {
            path: platform::PathScanner::new(root.clone(), opts),
            root,
            opts,
            processes: process_io::ProcessScanner::new(opts.pid),
            mode: opts.mode,
        }
    }
    pub fn scan(&mut self) -> Snapshot {
        match self.mode {
            Mode::Path => self.path.scan(),
            Mode::ReadHistory => self.processes.scan(),
        }
    }
}
pub fn sort_folders(folders: &mut [FolderActivity]) {
    folders.sort_by(|a, b| {
        b.bps()
            .total_cmp(&a.bps())
            .then_with(|| a.path.cmp(&b.path))
    });
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
                    Ok(Cmd::Mode(mode)) => {
                        scanner.mode = mode;
                        scanner.path =
                            platform::PathScanner::new(scanner.root.clone(), scanner.opts);
                        scanner.processes.reset();
                        break;
                    }
                    Ok(Cmd::Interval(d)) => {
                        interval = d;
                        deadline = started + d;
                    }
                }
            }
        }
    })
}
