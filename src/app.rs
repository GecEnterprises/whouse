use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::TableState;

use crate::scan::{self, Cmd, FolderActivity, ProcActivity, Snapshot};
use crate::target::Target;

const HISTORY: usize = 1000;
const PROC_HISTORY: usize = 240;
const INTERVALS_MS: [u64; 7] = [100, 250, 500, 1000, 2000, 5000, 10000];

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
    pub target: Target,
    pub snap: Option<Snapshot>,
    /// (read, write) bytes/s of the whole path, oldest first.
    pub history: VecDeque<(f64, f64)>,
    pub proc_history: HashMap<u32, VecDeque<(f64, f64)>>,

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
    pub quit: bool,
    cmds: Sender<Cmd>,
}

impl App {
    pub fn new(target: Target, interval: Duration, cmds: Sender<Cmd>) -> Self {
        Self {
            target,
            snap: None,
            history: VecDeque::new(),
            proc_history: HashMap::new(),
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
            quit: false,
            cmds,
        }
    }

    pub fn selected_proc(&self) -> Option<&ProcActivity> {
        let pid = self.selected?;
        self.snap.as_ref()?.procs.iter().find(|p| p.pid == pid)
    }

    pub fn on_snapshot(&mut self, snap: Snapshot) {
        if self.paused {
            return;
        }
        push_capped(
            &mut self.history,
            (snap.totals.read_bps, snap.totals.write_bps),
            HISTORY,
        );
        for p in &snap.procs {
            let h = self.proc_history.entry(p.pid).or_default();
            push_capped(h, (p.read_bps, p.write_bps), PROC_HISTORY);
        }
        self.proc_history
            .retain(|pid, _| snap.procs.iter().any(|p| p.pid == *pid));
        self.target.refresh_space();
        self.snap = Some(snap);
        self.rebuild_view();
    }

    /// Re-derive the visible, sorted process list and the folder list.
    pub fn rebuild_view(&mut self) {
        let Some(snap) = &self.snap else { return };
        let needle = self.filter.to_lowercase();
        let mut view: Vec<usize> = (0..snap.procs.len())
            .filter(|&i| {
                let p = &snap.procs[i];
                (!self.active_only || p.bps() > 0.0)
                    && (needle.is_empty() || matches_filter(p, &needle))
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

        let n_files = self.selected_proc().map_or(0, |p| p.files.len());
        let f = self.file_state.selected().unwrap_or(0);
        self.file_state
            .select((n_files > 0).then(|| f.min(n_files - 1)));
    }

    fn move_selection(&mut self, delta: isize) {
        if self.focus == Focus::Files {
            let n = self.selected_proc().map_or(0, |p| p.files.len());
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
        self.pinned = i != 0 || delta > 0;
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

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
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
                    if row < self.selected_proc().map_or(0, |p| p.files.len()) {
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
            ppid: 1,
            comm: comm.into(),
            cmdline: format!("/usr/bin/{comm} --flag"),
            user: "gec".into(),
            state: 'S',
            cwd: None,
            files,
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
}
