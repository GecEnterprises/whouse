use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::TableState;

use crate::scan::{self, Access, Cmd, FolderActivity, Kind, Mode, ProcActivity, Snapshot};
use crate::target::Target;

const HISTORY: usize = 1000;
const PROC_HISTORY: usize = 240;
const INTERVALS_MS: [u64; 7] = [100, 250, 500, 1000, 2000, 5000, 10000];

pub struct ReadSample {
    pub at: Instant,
    pub bps: f64,
    pub disk_bps: Option<f64>,
    pub total: u64,
    pub observed: bool,
}

pub struct FileVisit {
    pub path: PathBuf,
    pub kind: Kind,
    pub access: Access,
    pub first_seen: Instant,
    pub last_seen: Instant,
    pub last_read: Option<Instant>,
    pub last_write: Option<Instant>,
    pub open: Option<bool>,
    pub visits: u32,
    pub read_bps: f64,
    pub write_bps: f64,
    pub pos: u64,
    pub size: u64,
    pub deleted: bool,
    pub shared: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Io,
    Read,
    Write,
    Pid,
    Name,
    Files,
}

impl SortKey {
    const ALL: [SortKey; 6] = [
        SortKey::Io,
        SortKey::Read,
        SortKey::Write,
        SortKey::Pid,
        SortKey::Name,
        SortKey::Files,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Io => "i/o",
            SortKey::Read => "read",
            SortKey::Write => "write",
            SortKey::Pid => "pid",
            SortKey::Name => "name",
            SortKey::Files => "files",
        }
    }

    fn step(self, by: isize) -> Self {
        let i = Self::ALL.iter().position(|&k| k == self).unwrap_or(0) as isize;
        Self::ALL[(i + by).rem_euclid(Self::ALL.len() as isize) as usize]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Procs,
    Files,
}

pub struct App {
    pub mode: Mode,
    pub target: Target,
    pub snap: Option<Snapshot>,
    /// (read, write) bytes/s of the whole path, oldest first.
    pub history: VecDeque<(f64, f64)>,
    pub proc_history: HashMap<u32, VecDeque<(f64, f64)>>,
    pub read_history: HashMap<u32, VecDeque<ReadSample>>,
    pub file_history: HashMap<u32, Vec<FileVisit>>,
    identities: HashMap<u32, u64>,

    /// Indices into `snap.procs`, filtered and sorted for display.
    pub view: Vec<usize>,
    /// Folders, regrouped to `folder_depth` levels below the root (0 = as is).
    pub folders: Vec<FolderActivity>,
    pub folder_depth: usize,

    pub sort: SortKey,
    pub reverse: bool,
    pub active_only: bool,
    pub filter: String,
    pub editing_filter: bool,

    pub paused: bool,
    pub interval: Duration,
    pub selected: Option<u32>,
    /// Once the user moves the selection it sticks to that pid; until then it
    /// simply tracks the top row, so the busiest process is shown by default.
    pinned: bool,
    pub focus: Focus,
    pub proc_state: TableState,
    pub file_state: TableState,
    /// Screen areas of the two tables' rows, recorded while drawing for mouse hit-testing.
    pub proc_rows: Rect,
    pub file_rows: Rect,

    pub show_help: bool,
    pub show_file_details: bool,
    pub quit: bool,
    cmds: Sender<Cmd>,
}

impl App {
    pub fn new(target: Target, interval: Duration, cmds: Sender<Cmd>) -> Self {
        Self {
            mode: Mode::Path,
            target,
            snap: None,
            history: VecDeque::new(),
            proc_history: HashMap::new(),
            read_history: HashMap::new(),
            file_history: HashMap::new(),
            identities: HashMap::new(),
            view: Vec::new(),
            folders: Vec::new(),
            folder_depth: 0,
            sort: SortKey::Io,
            reverse: false,
            active_only: false,
            filter: String::new(),
            editing_filter: false,
            paused: false,
            interval,
            selected: None,
            pinned: false,
            focus: Focus::Procs,
            proc_state: TableState::default(),
            file_state: TableState::default(),
            proc_rows: Rect::default(),
            file_rows: Rect::default(),
            show_help: false,
            show_file_details: false,
            quit: false,
            cmds,
        }
    }

    pub fn selected_proc(&self) -> Option<&ProcActivity> {
        let pid = self.selected?;
        self.snap.as_ref()?.procs.iter().find(|p| p.pid == pid)
    }

    pub fn on_snapshot(&mut self, snap: Snapshot) {
        if self.paused || snap.mode != self.mode {
            return;
        }
        let selected_file = self.selected.and_then(|pid| {
            self.file_history
                .get(&pid)
                .and_then(|files| self.file_state.selected().and_then(|i| files.get(i)))
                .map(|file| (pid, file.path.clone()))
        });
        push_capped(
            &mut self.history,
            (snap.totals.read_bps, snap.totals.write_bps),
            HISTORY,
        );
        for p in &snap.procs {
            if self
                .identities
                .insert(p.pid, p.identity)
                .is_some_and(|old| old != p.identity)
            {
                self.proc_history.remove(&p.pid);
                self.read_history.remove(&p.pid);
                self.file_history.remove(&p.pid);
            }
            let h = self.proc_history.entry(p.pid).or_default();
            push_capped(h, (p.read_bps, p.write_bps), PROC_HISTORY);
            if self.mode == Mode::ReadHistory && p.io.is_some() {
                push_capped(
                    self.read_history.entry(p.pid).or_default(),
                    ReadSample {
                        at: Instant::now(),
                        bps: p.read_bps,
                        disk_bps: if cfg!(target_os = "linux") {
                            p.io.map(|io| io.read_bytes_bps)
                        } else {
                            None
                        },
                        total: p.read_total,
                        observed: p.io.is_some(),
                    },
                    PROC_HISTORY,
                );
            }
            if self.mode == Mode::ReadHistory {
                let now = Instant::now();
                let visits = self.file_history.entry(p.pid).or_default();
                let previously_open: std::collections::HashSet<_> = visits
                    .iter()
                    .filter(|v| v.open == Some(true))
                    .map(|v| v.path.clone())
                    .collect();
                for visit in visits.iter_mut() {
                    visit.open = if p.files_observed { Some(false) } else { None };
                    visit.read_bps = 0.0;
                    visit.write_bps = 0.0;
                }
                for file in &p.files {
                    let index = visits
                        .iter()
                        .position(|v| v.path == file.path)
                        .unwrap_or_else(|| {
                            visits.push(FileVisit {
                                path: file.path.clone(),
                                kind: file.kind,
                                access: file.access,
                                first_seen: now,
                                last_seen: now,
                                last_read: None,
                                last_write: None,
                                open: Some(false),
                                visits: 0,
                                read_bps: 0.0,
                                write_bps: 0.0,
                                pos: file.pos,
                                size: file.size,
                                deleted: file.deleted,
                                shared: file.shared,
                            });
                            visits.len() - 1
                        });
                    let visit = &mut visits[index];
                    if visit.open != Some(true) && !previously_open.contains(&file.path) {
                        visit.visits += 1;
                    }
                    visit.open = Some(true);
                    visit.last_seen = now;
                    // Multiple handles on one path are merged for the history list.
                    if visit.access != file.access {
                        visit.access = Access::ReadWrite;
                    }
                    visit.read_bps += file.read_bps;
                    visit.write_bps += file.write_bps;
                    if file.read_bps > 0.0 {
                        visit.last_read = Some(now);
                    }
                    if file.write_bps > 0.0 {
                        visit.last_write = Some(now);
                    }
                    visit.pos = file.pos;
                    visit.size = file.size;
                    visit.deleted = file.deleted;
                    visit.shared = file.shared;
                }
                visits.sort_by(|a, b| {
                    b.last_seen
                        .cmp(&a.last_seen)
                        .then_with(|| a.path.cmp(&b.path))
                });
                visits.truncate(256);
            }
        }
        self.proc_history
            .retain(|pid, _| snap.procs.iter().any(|p| p.pid == *pid));
        self.read_history
            .retain(|pid, _| snap.procs.iter().any(|p| p.pid == *pid));
        self.identities
            .retain(|pid, _| snap.procs.iter().any(|p| p.pid == *pid));
        self.file_history
            .retain(|pid, _| snap.procs.iter().any(|p| p.pid == *pid));
        self.target.refresh_space();
        self.snap = Some(snap);
        self.rebuild_view();
        if let Some((pid, path)) = selected_file.filter(|(pid, _)| self.selected == Some(*pid)) {
            if let Some(index) = self
                .file_history
                .get(&pid)
                .and_then(|files| files.iter().position(|f| f.path == path))
            {
                self.file_state.select(Some(index));
            }
        }
    }

    /// Re-derive the visible, sorted process list and the folder list.
    pub fn rebuild_view(&mut self) {
        let Some(snap) = &self.snap else { return };
        let needle = self.filter.to_lowercase();
        let mut view: Vec<usize> = (0..snap.procs.len())
            .filter(|&i| {
                let p = &snap.procs[i];
                (!self.active_only
                    || if self.mode == Mode::ReadHistory {
                        p.read_bps > 0.0
                    } else {
                        p.bps() > 0.0
                    })
                    && (needle.is_empty()
                        || matches_filter(p, &needle)
                        || self.file_history.get(&p.pid).is_some_and(|files| {
                            files
                                .iter()
                                .any(|f| f.path.to_string_lossy().to_lowercase().contains(&needle))
                        }))
            })
            .collect();

        let procs = &snap.procs;
        view.sort_by(|&a, &b| {
            let (a, b) = (&procs[a], &procs[b]);
            let ord = match self.sort {
                SortKey::Io => b
                    .bps()
                    .total_cmp(&a.bps())
                    .then(b.hint_bps().total_cmp(&a.hint_bps())),
                SortKey::Read => b
                    .read_bps
                    .total_cmp(&a.read_bps)
                    .then(b.hint_read_bps.total_cmp(&a.hint_read_bps)),
                SortKey::Write => b
                    .write_bps
                    .total_cmp(&a.write_bps)
                    .then(b.hint_write_bps.total_cmp(&a.hint_write_bps)),
                SortKey::Pid => a.pid.cmp(&b.pid),
                SortKey::Name => a.comm.to_lowercase().cmp(&b.comm.to_lowercase()),
                SortKey::Files => b.files.len().cmp(&a.files.len()),
            };
            let ord = ord.then(a.pid.cmp(&b.pid));
            if self.reverse { ord.reverse() } else { ord }
        });
        self.view = view;
        self.folders = group_folders(&snap.folders, &self.target.root, self.folder_depth);

        // Keep the selection on the same process while rows reorder.
        let pos = if self.pinned {
            self.selected
                .and_then(|pid| self.view.iter().position(|&i| procs[i].pid == pid))
        } else {
            (!self.view.is_empty()).then_some(0)
        };
        let idx = pos.or_else(|| {
            let last = self.proc_state.selected().unwrap_or(0);
            (!self.view.is_empty()).then(|| last.min(self.view.len() - 1))
        });
        self.selected = idx.map(|i| procs[self.view[i]].pid);
        self.proc_state.select(idx);

        let n_files = self.file_count();
        let f = self.file_state.selected().unwrap_or(0);
        self.file_state
            .select((n_files > 0).then(|| f.min(n_files - 1)));
    }

    fn move_selection(&mut self, delta: isize) {
        if self.focus == Focus::Files {
            let n = self.file_count();
            if n > 0 {
                let i = self.file_state.selected().unwrap_or(0) as isize;
                self.file_state
                    .select(Some((i + delta).clamp(0, n as isize - 1) as usize));
            }
            return;
        }
        if self.view.is_empty() {
            return;
        }
        let i = self.proc_state.selected().unwrap_or(0) as isize;
        let i = (i + delta).clamp(0, self.view.len() as isize - 1) as usize;
        self.pinned = self.mode == Mode::ReadHistory || i != 0 || delta > 0;
        self.proc_state.select(Some(i));
        let pid = self.snap.as_ref().map(|s| s.procs[self.view[i]].pid);
        if pid != self.selected {
            self.selected = pid;
            self.file_state.select(Some(0));
        }
    }

    fn jump(&mut self, to_end: bool) {
        self.move_selection(if to_end {
            isize::MAX / 2
        } else {
            isize::MIN / 2
        });
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if self.editing_filter {
            match key.code {
                KeyCode::Enter => self.editing_filter = false,
                KeyCode::Esc => {
                    self.editing_filter = false;
                    self.filter.clear();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter.push(c)
                }
                _ => {}
            }
            self.rebuild_view();
            return;
        }
        if self.show_help {
            self.show_help = false;
            return;
        }
        if self.show_file_details {
            self.show_file_details = false;
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Enter if self.mode == Mode::ReadHistory && self.focus == Focus::Files => {
                self.show_file_details = self.file_count() > 0;
            }
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild_view();
                } else {
                    self.focus = Focus::Procs;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Home => self.jump(false),
            KeyCode::End => self.jump(true),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Procs => Focus::Files,
                    Focus::Files => Focus::Procs,
                };
                if self.mode == Mode::ReadHistory {
                    self.pinned = true;
                }
            }
            KeyCode::Right | KeyCode::Char('s') => self.set_sort(self.sort.step(1)),
            KeyCode::Left => self.set_sort(self.sort.step(-1)),
            KeyCode::Char('r') => {
                self.reverse = !self.reverse;
                self.rebuild_view();
            }
            KeyCode::Char('i') => {
                self.active_only = !self.active_only;
                self.rebuild_view();
            }
            KeyCode::Char('g') => {
                self.folder_depth = (self.folder_depth + 1) % 4;
                self.rebuild_view();
            }
            KeyCode::Char('m') => {
                self.mode = if self.mode == Mode::Path {
                    Mode::ReadHistory
                } else {
                    Mode::Path
                };
                self.sort = if self.mode == Mode::ReadHistory {
                    SortKey::Read
                } else {
                    SortKey::Io
                };
                self.history.clear();
                self.proc_history.clear();
                self.read_history.clear();
                self.file_history.clear();
                self.identities.clear();
                self.snap = None;
                self.view.clear();
                self.folders.clear();
                self.selected = None;
                self.pinned = false;
                self.focus = Focus::Procs;
                self.paused = false;
                let _ = self.cmds.send(Cmd::Mode(self.mode));
            }
            KeyCode::Char('p') | KeyCode::Char(' ') => self.paused = !self.paused,
            KeyCode::Char('+') | KeyCode::Char('=') => self.step_interval(1),
            KeyCode::Char('-') | KeyCode::Char('_') => self.step_interval(-1),
            KeyCode::Char('R') | KeyCode::F(5) => {
                let _ = self.cmds.send(Cmd::Refresh);
            }
            KeyCode::Char('/') => self.editing_filter = true,
            KeyCode::Char('?') | KeyCode::Char('h') | KeyCode::F(1) => self.show_help = true,
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        let pos = Position::new(ev.column, ev.row);
        match ev.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let d = if ev.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                };
                if self.proc_rows.contains(pos) {
                    let keep = std::mem::replace(&mut self.focus, Focus::Procs);
                    self.move_selection(d);
                    self.focus = keep;
                } else if self.file_rows.contains(pos) {
                    let keep = std::mem::replace(&mut self.focus, Focus::Files);
                    self.move_selection(d);
                    self.focus = keep;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.proc_rows.contains(pos) {
                    let row = (pos.y - self.proc_rows.y) as usize + self.proc_state.offset();
                    if row < self.view.len() {
                        self.focus = Focus::Procs;
                        let cur = self.proc_state.selected().unwrap_or(0) as isize;
                        self.move_selection(row as isize - cur);
                    }
                } else if self.file_rows.contains(pos) {
                    let row = (pos.y - self.file_rows.y) as usize + self.file_state.offset();
                    if row < self.file_count() {
                        self.focus = Focus::Files;
                        self.file_state.select(Some(row));
                    }
                }
            }
            _ => {}
        }
    }

    fn set_sort(&mut self, key: SortKey) {
        self.sort = key;
        self.rebuild_view();
    }

    fn file_count(&self) -> usize {
        if self.mode == Mode::ReadHistory {
            self.selected
                .and_then(|pid| self.file_history.get(&pid))
                .map_or(0, Vec::len)
        } else {
            self.selected_proc().map_or(0, |p| p.files.len())
        }
    }

    fn step_interval(&mut self, by: isize) {
        let ms = self.interval.as_millis() as u64;
        let i = INTERVALS_MS
            .iter()
            .position(|&m| m >= ms)
            .unwrap_or(INTERVALS_MS.len() - 1) as isize;
        let next = INTERVALS_MS[(i + by).clamp(0, INTERVALS_MS.len() as isize - 1) as usize];
        self.interval = Duration::from_millis(next);
        let _ = self.cmds.send(Cmd::Interval(self.interval));
    }
}

fn push_capped<T>(q: &mut VecDeque<T>, v: T, cap: usize) {
    if q.len() == cap {
        q.pop_front();
    }
    q.push_back(v);
}

fn matches_filter(p: &ProcActivity, needle: &str) -> bool {
    p.comm.to_lowercase().contains(needle)
        || p.cmdline.to_lowercase().contains(needle)
        || p.user.to_lowercase().contains(needle)
        || p.pid.to_string().contains(needle)
        || p.files
            .iter()
            .any(|f| f.path.to_string_lossy().to_lowercase().contains(needle))
}

/// Collapse folders deeper than `depth` levels below `root` into their ancestor.
fn group_folders(
    folders: &[FolderActivity],
    root: &std::path::Path,
    depth: usize,
) -> Vec<FolderActivity> {
    if depth == 0 {
        return folders.to_vec();
    }
    let mut grouped: HashMap<PathBuf, FolderActivity> = HashMap::new();
    for f in folders {
        let key = match f.path.strip_prefix(root) {
            Ok(rel) => {
                let mut p = root.to_path_buf();
                p.extend(rel.components().take(depth));
                p
            }
            Err(_) => f.path.clone(),
        };
        let g = grouped
            .entry(key.clone())
            .or_insert_with(|| FolderActivity {
                path: key,
                ..FolderActivity::default()
            });
        g.read_bps += f.read_bps;
        g.write_bps += f.write_bps;
        g.files += f.files;
        g.opened += f.opened;
        for pid in &f.pids {
            if !g.pids.contains(pid) {
                g.pids.push(*pid);
            }
        }
    }
    let mut out: Vec<FolderActivity> = grouped.into_values().collect();
    scan::sort_folders(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Access, Kind, OpenFile, Totals};
    use std::path::Path;
    use std::sync::mpsc;

    fn file(path: &str, read_bps: f64) -> OpenFile {
        OpenFile {
            fds: vec![3],
            path: path.into(),
            kind: Kind::File,
            access: Access::Read,
            pos: 0,
            size: 0,
            read_bps,
            write_bps: 0.0,
            deleted: false,
            shared: false,
        }
    }

    fn proc(pid: u32, comm: &str, read_bps: f64, files: Vec<OpenFile>) -> ProcActivity {
        ProcActivity {
            pid,
            identity: pid as u64,
            ppid: 1,
            comm: comm.into(),
            cmdline: format!("/usr/bin/{comm} --flag"),
            user: "gec".into(),
            state: 'S',
            cwd: None,
            files,
            files_observed: true,
            read_bps,
            write_bps: 0.0,
            read_total: 0,
            write_total: 0,
            io: None,
            hint_read_bps: 0.0,
            hint_write_bps: 0.0,
            focus: None,
            idle_for: None,
        }
    }

    fn snap(procs: Vec<ProcActivity>) -> Snapshot {
        Snapshot {
            procs,
            folders: Vec::new(),
            totals: Totals::default(),
            scanned: 0,
            denied: 0,
            scan_time: Duration::ZERO,
            mode: Mode::Path,
            warning: None,
        }
    }

    fn app() -> App {
        let (tx, _rx) = mpsc::channel();
        App::new(
            Target::new(PathBuf::from("/tmp")),
            Duration::from_millis(500),
            tx,
        )
    }

    fn pids(app: &App) -> Vec<u32> {
        let s = app.snap.as_ref().unwrap();
        app.view.iter().map(|&i| s.procs[i].pid).collect()
    }

    #[test]
    fn sorts_filters_and_keeps_selection_on_the_same_pid() {
        let mut a = app();
        a.on_snapshot(snap(vec![
            proc(10, "alpha", 5.0, vec![file("/tmp/x/one", 5.0)]),
            proc(20, "beta", 9.0, vec![]),
            proc(30, "gamma", 0.0, vec![]),
        ]));
        assert_eq!(pids(&a), [20, 10, 30], "busiest first");
        assert_eq!(a.selected, Some(20));

        a.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(a.selected, Some(10));

        // The ranking flips, selection must follow pid 10, not row 1.
        a.on_snapshot(snap(vec![
            proc(10, "alpha", 50.0, vec![]),
            proc(20, "beta", 9.0, vec![]),
            proc(30, "gamma", 0.0, vec![]),
        ]));
        assert_eq!(pids(&a), [10, 20, 30]);
        assert_eq!(a.selected, Some(10));
        assert_eq!(a.proc_state.selected(), Some(0));

        a.on_key(KeyEvent::from(KeyCode::Char('r')));
        assert_eq!(pids(&a), [30, 20, 10]);
        a.on_key(KeyEvent::from(KeyCode::Char('i')));
        assert_eq!(pids(&a), [20, 10], "active only hides idle rows");
        a.on_key(KeyEvent::from(KeyCode::Char('i')));

        for c in "/GAM".chars() {
            a.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(pids(&a), [30], "filter is case-insensitive");
        a.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(a.selected, Some(30));
        a.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(pids(&a).len(), 3);
    }

    #[test]
    fn selection_survives_the_selected_process_vanishing() {
        let mut a = app();
        a.on_snapshot(snap(vec![
            proc(1, "a", 3.0, vec![]),
            proc(2, "b", 2.0, vec![]),
            proc(3, "c", 1.0, vec![]),
        ]));
        a.on_key(KeyEvent::from(KeyCode::End));
        assert_eq!(a.selected, Some(3));
        a.on_snapshot(snap(vec![
            proc(1, "a", 3.0, vec![]),
            proc(2, "b", 2.0, vec![]),
        ]));
        assert_eq!(a.selected, Some(2), "clamps to the last row");
        a.on_snapshot(snap(vec![]));
        assert_eq!(a.selected, None);
        a.on_key(KeyEvent::from(KeyCode::Down));
    }

    #[test]
    fn pause_freezes_history_and_interval_steps_through_presets() {
        let mut a = app();
        a.on_snapshot(snap(vec![]));
        a.on_key(KeyEvent::from(KeyCode::Char('p')));
        a.on_snapshot(snap(vec![]));
        assert_eq!(a.history.len(), 1);

        a.on_key(KeyEvent::from(KeyCode::Char('+')));
        assert_eq!(a.interval, Duration::from_millis(1000));
        a.on_key(KeyEvent::from(KeyCode::Char('-')));
        a.on_key(KeyEvent::from(KeyCode::Char('-')));
        assert_eq!(a.interval, Duration::from_millis(250));
        for _ in 0..5 {
            a.on_key(KeyEvent::from(KeyCode::Char('-')));
        }
        assert_eq!(a.interval, Duration::from_millis(100));
    }

    #[test]
    fn groups_folders_by_depth() {
        let f = |p: &str, bps: f64, pid: u32| FolderActivity {
            path: p.into(),
            read_bps: bps,
            pids: vec![pid],
            files: 1,
            ..FolderActivity::default()
        };
        let folders = [
            f("/m/a/x", 1.0, 1),
            f("/m/a/y", 2.0, 1),
            f("/m/b", 1.0, 2),
            f("/m", 0.0, 3),
        ];
        let g = group_folders(&folders, Path::new("/m"), 1);
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].path, PathBuf::from("/m/a"));
        assert_eq!(g[0].read_bps, 3.0);
        assert_eq!(g[0].pids, [1], "same pid in two subfolders counts once");
        assert_eq!(g[0].files, 2);
    }

    #[test]
    fn mode_switch_rejects_stale_snapshots_and_resets_history() {
        let (tx, rx) = mpsc::channel();
        let mut a = App::new(
            Target::new(PathBuf::from(".")),
            Duration::from_millis(500),
            tx,
        );
        a.on_snapshot(snap(vec![proc(1, "reader", 100.0, vec![])]));
        a.on_key(KeyEvent::from(KeyCode::Char('m')));
        assert_eq!(a.mode, Mode::ReadHistory);
        assert_eq!(a.sort.label(), "read");
        assert!(matches!(
            rx.try_recv().unwrap(),
            Cmd::Mode(Mode::ReadHistory)
        ));
        assert!(a.history.is_empty());
        a.on_snapshot(snap(vec![proc(1, "reader", 100.0, vec![])]));
        assert!(a.snap.is_none(), "queued path snapshots must be ignored");
        let mut s = snap(vec![proc(1, "reader", 100.0, vec![])]);
        s.mode = Mode::ReadHistory;
        s.procs[0].io = Some(crate::scan::ProcIo::default());
        a.on_snapshot(s);
        assert_eq!(a.read_history[&1].len(), 1);
        a.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(a.focus, Focus::Files);
    }

    #[test]
    fn read_history_survives_unobservable_process_and_resets_on_pid_reuse() {
        let mut a = app();
        a.mode = Mode::ReadHistory;
        let mut s = snap(vec![proc(42, "reader", 100.0, vec![])]);
        s.mode = Mode::ReadHistory;
        s.procs[0].io = Some(crate::scan::ProcIo::default());
        for _ in 0..300 {
            a.on_snapshot(s.clone());
        }
        assert_eq!(a.read_history[&42].len(), PROC_HISTORY);
        let mut unseen = s.clone();
        unseen.procs[0].io = None;
        unseen.procs[0].read_bps = 0.0;
        a.on_snapshot(unseen);
        assert_eq!(a.read_history[&42].back().unwrap().bps, 100.0);
        s.procs[0].identity += 1;
        a.on_snapshot(s);
        assert_eq!(a.read_history[&42].len(), 1);
    }

    #[test]
    fn file_history_keeps_closed_files_and_unknown_visibility() {
        let mut a = app();
        a.mode = Mode::ReadHistory;
        let mut snapshot = snap(vec![proc(
            42,
            "reader",
            100.0,
            vec![file("/tmp/data.bin", 100.0)],
        )]);
        snapshot.mode = Mode::ReadHistory;
        snapshot.procs[0].io = Some(crate::scan::ProcIo::default());
        a.on_snapshot(snapshot.clone());
        let visit = &a.file_history[&42][0];
        assert_eq!(visit.open, Some(true));
        assert_eq!(visit.visits, 1);
        assert!(visit.last_read.is_some());
        snapshot.procs[0].files.clear();
        a.on_snapshot(snapshot.clone());
        assert_eq!(a.file_history[&42][0].open, Some(false));
        a.filter = "data.bin".into();
        a.rebuild_view();
        assert_eq!(pids(&a), [42], "closed file paths can be searched");
        snapshot.procs[0].files_observed = false;
        a.on_snapshot(snapshot.clone());
        assert_eq!(a.file_history[&42][0].open, None);
        snapshot.procs[0].files_observed = true;
        snapshot.procs[0].files.push(file("/tmp/data.bin", 0.0));
        a.on_snapshot(snapshot);
        assert_eq!(a.file_history[&42][0].visits, 2);
        a.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(a.file_count(), 1);
        a.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(a.file_state.selected(), Some(0));
    }
}
