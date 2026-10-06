use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Table, Wrap,
};

use crate::app::{App, Focus, SortKey};
use crate::graph::{Graph, Grow, Stops, gradient};
use crate::scan::{Access, Kind, Mode, OpenFile};
use crate::util::{fmt_bytes, fmt_rate, truncate_left, truncate_right};

const TEXT: Color = Color::Rgb(0xcc, 0xcc, 0xcc);
const DIM: Color = Color::Rgb(0x73, 0x73, 0x73);
const FAINT: Color = Color::Rgb(0x48, 0x48, 0x48);
const TITLE: Color = Color::Rgb(0xee, 0xee, 0xee);
const HOTKEY: Color = Color::Rgb(0xdc, 0x4c, 0x4c);
const WARN: Color = Color::Rgb(0xe0, 0xb0, 0x50);
const SELECTED: Color = Color::Rgb(0x5a, 0x2a, 0x2a);
const SELECTED_DIM: Color = Color::Rgb(0x2e, 0x24, 0x24);

const BORDER_GRAPH: Color = Color::Rgb(0x55, 0x6d, 0x59);
const BORDER_TARGET: Color = Color::Rgb(0x5c, 0x58, 0x8d);
const BORDER_PROCS: Color = Color::Rgb(0x80, 0x52, 0x52);
const BORDER_FOLDERS: Color = Color::Rgb(0x6c, 0x6c, 0x4b);
const BORDER_DETAIL: Color = Color::Rgb(0x4f, 0x6f, 0x86);

const READ: Stops = [(0x77, 0xca, 0x9b), (0xcb, 0xc0, 0x6c), (0xdc, 0x4c, 0x4c)];
const WRITE: Stops = [(0x5e, 0xa4, 0xd6), (0x8a, 0x7f, 0xd8), (0xd9, 0x6a, 0xd0)];
const USAGE: Stops = [(0x77, 0xca, 0x9b), (0xcb, 0xc0, 0x6c), (0xdc, 0x4c, 0x4c)];

const MIN_SCALE: f64 = 1024.0 * 1024.0;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < 70 || area.height < 20 {
        let msg = format!(
            "terminal too small: {}x{}, need at least 70x20",
            area.width, area.height
        );
        f.render_widget(
            Paragraph::new(msg)
                .alignment(Alignment::Center)
                .style(Style::new().fg(WARN)),
            area,
        );
        return;
    }

    let [main, footer] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    if app.mode == Mode::ReadHistory {
        draw_read_history(f, app, main);
        draw_footer(f, app, footer);
        if app.show_help {
            draw_help(f, area);
        }
        if app.show_file_details {
            draw_file_details(f, app, area);
        }
        return;
    }
    let top_h = (main.height / 3).clamp(8, 14);
    let [top, bottom] =
        Layout::vertical([Constraint::Length(top_h), Constraint::Fill(1)]).areas(main);
    let wide = area.width >= 110;

    let info_w = if wide {
        Constraint::Length(50)
    } else {
        Constraint::Percentage(45)
    };
    let [graph, info] = Layout::horizontal([Constraint::Fill(1), info_w]).areas(top);
    draw_throughput(f, app, graph);
    draw_target(f, app, info);

    if wide {
        let [procs, side] =
            Layout::horizontal([Constraint::Percentage(56), Constraint::Fill(1)]).areas(bottom);
        let [folders, detail] =
            Layout::vertical([Constraint::Percentage(36), Constraint::Fill(1)]).areas(side);
        draw_procs(f, app, procs);
        draw_folders(f, app, folders);
        draw_detail(f, app, detail);
    } else {
        let [procs, detail] =
            Layout::vertical([Constraint::Percentage(52), Constraint::Fill(1)]).areas(bottom);
        draw_procs(f, app, procs);
        draw_detail(f, app, detail);
    }
    draw_footer(f, app, footer);
    if app.show_help {
        draw_help(f, area);
    }
}

// --- shared pieces ----------------------------------------------------------

fn panel(title: &str, border: Color) -> Block<'static> {
    let edge = Style::new().fg(border);
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(edge)
        .title(Line::from(vec![
            Span::styled("┤ ", edge),
            Span::styled(
                title.to_string(),
                Style::new().fg(TITLE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ├", edge),
        ]))
}

fn edge_title(spans: Vec<Span<'static>>, border: Color) -> Line<'static> {
    let edge = Style::new().fg(border);
    let mut all = vec![Span::styled("┤ ", edge)];
    all.extend(spans);
    all.push(Span::styled(" ├", edge));
    Line::from(all).right_aligned()
}

/// Colour that warms up with the order of magnitude of a rate.
fn heat(bps: f64, stops: Stops) -> Color {
    if bps < 1.0 {
        DIM
    } else {
        gradient(stops, bps.log10().clamp(0.0, 9.0) / 9.0)
    }
}

fn rate_text(bps: f64) -> String {
    if bps < 1.0 { "-".into() } else { fmt_rate(bps) }
}

fn rate_cell(bps: f64, stops: Stops, dimmed: bool) -> Cell<'static> {
    let color = if dimmed { DIM } else { heat(bps, stops) };
    Cell::from(Line::from(Span::styled(rate_text(bps), Style::new().fg(color))).right_aligned())
}

/// A measured rate, or when nothing could be measured but the process is
/// moving data it might be taking from the path, an upper bound in muted italics.
fn rate_or_bound_cell(measured: f64, bound: f64, stops: Stops, dimmed: bool) -> Cell<'static> {
    if measured < 1.0 && bound > 0.0 && !dimmed {
        let style = Style::new().fg(DIM).add_modifier(Modifier::ITALIC);
        return Cell::from(
            Line::from(Span::styled(format!("≤ {}", fmt_rate(bound)), style)).right_aligned(),
        );
    }
    rate_cell(measured, stops, dimmed)
}

fn label(s: &str) -> Span<'static> {
    Span::styled(format!("{s:<7}"), Style::new().fg(DIM))
}

fn meter(width: usize, frac: f64) -> Vec<Span<'static>> {
    let filled = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    let mut spans: Vec<Span<'static>> = (0..filled)
        .map(|i| {
            Span::styled(
                "█",
                Style::new().fg(gradient(USAGE, (i as f64 + 0.5) / width as f64)),
            )
        })
        .collect();
    spans.push(Span::styled(
        "░".repeat(width - filled),
        Style::new().fg(FAINT),
    ));
    spans
}

fn empty_message(f: &mut Frame, area: Rect, text: &str) {
    if area.is_empty() {
        return;
    }
    let y = area.y + area.height.saturating_sub(1) / 2;
    f.render_widget(
        Paragraph::new(text.to_string())
            .alignment(Alignment::Center)
            .style(Style::new().fg(DIM)),
        Rect::new(area.x, y, area.width, 1),
    );
}

// --- throughput -------------------------------------------------------------

fn draw_throughput(f: &mut Frame, app: &App, area: Rect) {
    let (read, write) = app
        .snap
        .as_ref()
        .map_or((0.0, 0.0), |s| (s.totals.read_bps, s.totals.write_bps));

    let samples = (area.width.saturating_sub(2) as usize) * 2;
    let series = |pick: fn(&(f64, f64)) -> f64| -> Vec<f64> {
        let mut v: Vec<f64> = app.history.iter().rev().take(samples).map(pick).collect();
        v.reverse();
        v
    };
    let (reads, writes) = (series(|s| s.0), series(|s| s.1));
    // Power-of-two scale so the axis label stays put while values wobble.
    let peak = reads
        .iter()
        .chain(&writes)
        .copied()
        .fold(0.0, f64::max)
        .max(MIN_SCALE);
    let scale = 2f64.powf(peak.log2().ceil());

    let block = panel("throughput", BORDER_GRAPH)
        .title(edge_title(
            vec![
                Span::styled("▲ read ", Style::new().fg(DIM)),
                Span::styled(
                    rate_text(read),
                    Style::new()
                        .fg(heat(read, READ))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled("▼ write ", Style::new().fg(DIM)),
                Span::styled(
                    rate_text(write),
                    Style::new()
                        .fg(heat(write, WRITE))
                        .add_modifier(Modifier::BOLD),
                ),
            ],
            BORDER_GRAPH,
        ))
        .title_bottom(edge_title(
            vec![Span::styled(
                format!("scale {}", fmt_rate(scale)),
                Style::new().fg(DIM),
            )],
            BORDER_GRAPH,
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let [upper, lower] = Layout::vertical([Constraint::Fill(1), Constraint::Fill(1)]).areas(inner);
    f.render_widget(
        Graph {
            data: &reads,
            max: scale,
            grow: Grow::Up,
            stops: READ,
        },
        upper,
    );
    f.render_widget(
        Graph {
            data: &writes,
            max: scale,
            grow: Grow::Down,
            stops: WRITE,
        },
        lower,
    );
    let tag =
        |text: &str, color: Color| Paragraph::new(text.to_string()).style(Style::new().fg(color));
    f.render_widget(
        tag("▲ read", gradient(READ, 0.0)),
        Rect {
            height: 1,
            width: 8.min(upper.width),
            ..upper
        },
    );
    f.render_widget(
        tag("▼ write", gradient(WRITE, 0.0)),
        Rect {
            y: lower.bottom() - 1,
            height: 1,
            width: 9.min(lower.width),
            ..lower
        },
    );
}

// --- target info ------------------------------------------------------------

fn draw_target(f: &mut Frame, app: &App, area: Rect) {
    let block = panel("target", BORDER_TARGET);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width.saturating_sub(8) as usize;
    let t = &app.target;
    let bold = Style::new().fg(TITLE).add_modifier(Modifier::BOLD);
    let plain = Style::new().fg(TEXT);
    let dim = Style::new().fg(DIM);

    let mut lines = vec![Line::from(vec![
        label(if t.is_dir { "folder" } else { "file" }),
        Span::styled(truncate_left(&t.root.to_string_lossy(), w), bold),
    ])];
    if let Some(m) = &t.mount {
        let fs = format!("  {}", m.fstype);
        let point_w = w.saturating_sub(fs.chars().count());
        lines.push(Line::from(vec![
            label("mount"),
            Span::styled(truncate_left(&m.point.to_string_lossy(), point_w), plain),
            Span::styled(fs, dim),
        ]));
        lines.push(Line::from(vec![
            label("device"),
            Span::styled(truncate_left(&m.source, w), plain),
        ]));
    }
    if let Some(sp) = t.space {
        let mut spans = vec![label("space")];
        spans.extend(meter(14, sp.used_fraction()));
        spans.push(Span::styled(
            format!(
                " {:>3.0}%  {} free",
                sp.used_fraction() * 100.0,
                fmt_bytes(sp.free)
            ),
            plain,
        ));
        lines.push(Line::from(spans));
    }
    lines.push(Line::raw(""));

    if let Some(s) = &app.snap {
        let n = |v: usize, what: &str| {
            vec![
                Span::styled(v.to_string(), bold),
                Span::styled(format!(" {what}   "), dim),
            ]
        };
        let mut spans = vec![label("open")];
        spans.extend(n(s.totals.procs, "procs"));
        spans.extend(n(s.totals.files, "files"));
        spans.extend(n(s.totals.dirs, "dirs"));
        spans.extend(n(s.totals.maps, "maps"));
        lines.push(Line::from(spans));
        lines.push(Line::from(vec![
            label("moved"),
            Span::styled("▲ ", Style::new().fg(gradient(READ, 0.0))),
            Span::styled(fmt_bytes(s.totals.read_total), plain),
            Span::styled("   ▼ ", Style::new().fg(gradient(WRITE, 0.0))),
            Span::styled(fmt_bytes(s.totals.write_total), plain),
            Span::styled("   since start", dim),
        ]));
        let mut scan = vec![
            label("scan"),
            Span::styled(
                format!(
                    "{} procs in {} ms, every {} ms",
                    s.scanned,
                    s.scan_time.as_millis(),
                    app.interval.as_millis()
                ),
                dim,
            ),
        ];
        if app.paused {
            scan.push(Span::styled("  ▌▌ paused", Style::new().fg(WARN)));
        }
        lines.push(Line::from(scan));
        if s.denied > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} unreadable processes, run as {}",
                    s.denied,
                    if cfg!(windows) {
                        "Administrator"
                    } else {
                        "root"
                    }
                ),
                Style::new().fg(WARN),
            )));
        }
        if let Some(warning) = &s.warning {
            lines.insert(0, Line::styled(warning.clone(), Style::new().fg(WARN)));
        }
    } else {
        lines.push(Line::from(Span::styled("scanning…", dim)));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

// --- processes --------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Col {
    Pid,
    Name,
    User,
    Read,
    Write,
    Files,
    Folder,
}

/// Always PID, program and both rates; then, as space allows, the folder, the
/// file count and the user. Returned in display order.
fn columns(width: u16) -> Vec<Col> {
    let mut cols = vec![Col::Pid, Col::Name, Col::Read, Col::Write];
    let needed = |cols: &[Col]| cols.iter().map(|&c| min_width(c) + 1).sum::<u16>() + 1;
    for cand in [Col::Folder, Col::Files, Col::User] {
        if needed(&cols) + min_width(cand) < width {
            cols.push(cand);
        }
    }
    let order = |c: &Col| match c {
        Col::Pid => 0,
        Col::Name => 1,
        Col::User => 2,
        Col::Read => 3,
        Col::Write => 4,
        Col::Files => 5,
        Col::Folder => 6,
    };
    cols.sort_by_key(order);
    cols
}

fn min_width(c: Col) -> u16 {
    if c == Col::Folder { 16 } else { col_width(c) }
}

fn col_width(c: Col) -> u16 {
    match c {
        Col::Pid => 7,
        Col::Name => 14,
        Col::User => 9,
        Col::Read | Col::Write => 12,
        Col::Files => 5,
        Col::Folder => 0,
    }
}

fn col_title(c: Col) -> &'static str {
    match c {
        Col::Pid => "PID",
        Col::Name => "Program",
        Col::User => "User",
        Col::Read => "Read/s",
        Col::Write => "Write/s",
        Col::Files => "Files",
        Col::Folder => "Folder",
    }
}

fn sorted_by(sort: SortKey, c: Col) -> bool {
    matches!(
        (sort, c),
        (SortKey::Io, Col::Read | Col::Write)
            | (SortKey::Read, Col::Read)
            | (SortKey::Write, Col::Write)
            | (SortKey::Pid, Col::Pid)
            | (SortKey::Name, Col::Name)
            | (SortKey::Files, Col::Files)
    )
}

fn draw_procs(f: &mut Frame, app: &mut App, area: Rect) {
    let total = app.snap.as_ref().map_or(0, |s| s.procs.len());
    let mut right = vec![Span::styled(
        format!("{}/{}", app.view.len(), total),
        Style::new().fg(DIM),
    )];
    if !app.filter.is_empty() {
        right.insert(
            0,
            Span::styled(format!("filter “{}”  ", app.filter), Style::new().fg(WARN)),
        );
    }
    if app.active_only {
        right.insert(0, Span::styled("active only  ", Style::new().fg(WARN)));
    }
    let block = panel("processes", BORDER_PROCS).title(edge_title(right, BORDER_PROCS));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.proc_rows = Rect::default();
    if inner.is_empty() {
        return;
    }

    if app.view.is_empty() {
        let msg = match &app.snap {
            None => "scanning…".to_string(),
            Some(_) if !app.filter.is_empty() => format!("no process matches “{}”", app.filter),
            Some(_) if app.active_only => "nothing is moving data right now".to_string(),
            Some(_) => format!("nothing has {} open right now", app.target.root.display()),
        };
        empty_message(f, inner, &msg);
        return;
    }

    let cols = columns(inner.width);
    let fixed: u16 = cols.iter().map(|&c| col_width(c) + 1).sum();
    let flexible = inner.width.saturating_sub(fixed + 1) as usize;
    let has_folder = cols.contains(&Col::Folder);

    let header = Row::new(cols.iter().map(|&c| {
        let mut title = col_title(c).to_string();
        let active = sorted_by(app.sort, c);
        if active {
            title.push_str(if app.reverse { " ▲" } else { " ▼" });
        }
        let style = Style::new()
            .fg(if active { HOTKEY } else { TITLE })
            .add_modifier(Modifier::BOLD);
        let line = Line::from(Span::styled(title, style));
        Cell::from(
            if matches!(c, Col::Pid | Col::Read | Col::Write | Col::Files) {
                line.right_aligned()
            } else {
                line
            },
        )
    }));

    let Some(snap) = &app.snap else { return };
    let rows: Vec<Row> = app
        .view
        .iter()
        .map(|&i| {
            let p = &snap.procs[i];
            let idle = p.idle_for.is_some();
            let fg = if idle { DIM } else { TEXT };
            let cells = cols.iter().map(|&c| match c {
                Col::Pid => Cell::from(Line::from(p.pid.to_string()).right_aligned())
                    .style(Style::new().fg(DIM)),
                Col::Name => {
                    let w = if has_folder {
                        col_width(Col::Name) as usize
                    } else {
                        flexible + col_width(Col::Name) as usize
                    };
                    Cell::from(truncate_right(&p.comm, w)).style(Style::new().fg(fg).add_modifier(
                        if idle {
                            Modifier::ITALIC
                        } else {
                            Modifier::empty()
                        },
                    ))
                }
                Col::User => Cell::from(truncate_right(&p.user, 9)).style(Style::new().fg(DIM)),
                Col::Read => rate_or_bound_cell(p.read_bps, p.hint_read_bps, READ, idle),
                Col::Write => rate_or_bound_cell(p.write_bps, p.hint_write_bps, WRITE, idle),
                Col::Files => {
                    let n = p.count(Kind::File) + p.count(Kind::Map);
                    Cell::from(
                        Line::from(if n == 0 {
                            "-".to_string()
                        } else {
                            n.to_string()
                        })
                        .right_aligned(),
                    )
                    .style(Style::new().fg(if n == 0 { DIM } else { fg }))
                }
                Col::Folder => {
                    let text = p
                        .focus
                        .as_ref()
                        .map_or_else(String::new, |d| app.target.rel(d));
                    Cell::from(truncate_left(&text, flexible)).style(Style::new().fg(DIM))
                }
            });
            Row::new(cells)
        })
        .collect();

    let widths: Vec<Constraint> = cols
        .iter()
        .map(|&c| match c {
            Col::Folder => Constraint::Fill(1),
            Col::Name if !has_folder => Constraint::Fill(1),
            _ => Constraint::Length(col_width(c)),
        })
        .collect();
    let highlight = if app.focus == Focus::Procs {
        SELECTED
    } else {
        SELECTED_DIM
    };
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .row_highlight_style(Style::new().bg(highlight).add_modifier(Modifier::BOLD));
    let body = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    app.proc_rows = body;
    f.render_stateful_widget(table, inner, &mut app.proc_state);

    let overflow = app.view.len().saturating_sub(body.height as usize);
    if overflow > 0 {
        let mut sb = ScrollbarState::new(overflow).position(app.proc_state.offset());
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_style(Style::new().fg(BORDER_PROCS)),
            area.inner(Margin::new(0, 1)),
            &mut sb,
        );
    }
}

// --- folders ----------------------------------------------------------------

fn draw_folders(f: &mut Frame, app: &App, area: Rect) {
    let depth = match app.folder_depth {
        0 => "full depth".to_string(),
        n => format!("{n} level{}", if n == 1 { "" } else { "s" }),
    };
    let block = panel("folders", BORDER_FOLDERS).title(edge_title(
        vec![Span::styled(format!("g: {depth}"), Style::new().fg(DIM))],
        BORDER_FOLDERS,
    ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    if app.folders.is_empty() {
        empty_message(f, inner, "no folders in use");
        return;
    }

    let fixed = 2 + 11 + 11 + 5 + 5 + 5;
    let name_w = inner.width.saturating_sub(fixed) as usize;
    let right = |s: &str| {
        Cell::from(
            Line::from(Span::styled(
                s.to_string(),
                Style::new().fg(TITLE).add_modifier(Modifier::BOLD),
            ))
            .right_aligned(),
        )
    };
    let header = Row::new(vec![
        Cell::from(""),
        Cell::from(Span::styled(
            "Folder",
            Style::new().fg(TITLE).add_modifier(Modifier::BOLD),
        )),
        right("Read/s"),
        right("Write/s"),
        right("Procs"),
        right("Files"),
    ]);
    let me = app.selected;
    let rows: Vec<Row> = app
        .folders
        .iter()
        .map(|d| {
            let mine = me.is_some_and(|pid| d.pids.contains(&pid));
            let count = |n: usize| {
                Cell::from(
                    Line::from(if n == 0 {
                        "-".to_string()
                    } else {
                        n.to_string()
                    })
                    .right_aligned(),
                )
                .style(Style::new().fg(if n == 0 { DIM } else { TEXT }))
            };
            Row::new(vec![
                Cell::from(if mine { "●" } else { " " }).style(Style::new().fg(HOTKEY)),
                Cell::from(truncate_left(&app.target.rel(&d.path), name_w))
                    .style(Style::new().fg(if d.bps() > 0.0 || mine { TEXT } else { DIM })),
                rate_cell(d.read_bps, READ, false),
                rate_cell(d.write_bps, WRITE, false),
                count(d.pids.len()),
                count(d.files + d.opened),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(11),
        Constraint::Length(11),
        Constraint::Length(5),
        Constraint::Length(5),
    ];
    f.render_widget(
        Table::new(rows, widths).header(header).column_spacing(1),
        inner,
    );
}

// --- detail -----------------------------------------------------------------

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(p) = app.selected_proc().cloned() else {
        let block = panel("detail", BORDER_DETAIL);
        let inner = block.inner(area);
        f.render_widget(block, area);
        app.file_rows = Rect::default();
        empty_message(f, inner, "no process selected");
        return;
    };

    let mut block = panel(&format!("pid {} · {}", p.pid, p.comm), BORDER_DETAIL);
    if let Some(idle) = p.idle_for {
        block = block.title(edge_title(
            vec![Span::styled(
                format!("idle {}s", idle.as_secs()),
                Style::new().fg(WARN),
            )],
            BORDER_DETAIL,
        ));
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let info_h = 6.min(inner.height);
    let [info, files] =
        Layout::vertical([Constraint::Length(info_h), Constraint::Fill(1)]).areas(inner);
    let w = info.width.saturating_sub(7) as usize;
    let dim = Style::new().fg(DIM);
    let plain = Style::new().fg(TEXT);

    let history: Vec<(f64, f64)> = app
        .proc_history
        .get(&p.pid)
        .map(|h| h.iter().copied().collect())
        .unwrap_or_default();
    let reads: Vec<f64> = history.iter().map(|h| h.0).collect();
    let writes: Vec<f64> = history.iter().map(|h| h.1).collect();
    let scale = reads
        .iter()
        .chain(&writes)
        .copied()
        .fold(0.0, f64::max)
        .max(64.0 * 1024.0);

    let mut lines = vec![
        Line::from(vec![
            label("user"),
            Span::styled(p.user.clone(), plain),
            Span::styled(format!("   state {}   ppid {}", p.state, p.ppid), dim),
        ]),
        Line::from(vec![
            label("cmd"),
            Span::styled(truncate_right(&p.cmdline, w), plain),
        ]),
        Line::from(vec![
            label("cwd"),
            Span::styled(
                p.cwd.as_ref().map_or_else(
                    || "outside, or not under the path".to_string(),
                    |c| app.target.rel(c),
                ),
                dim,
            ),
        ]),
    ];
    lines.truncate(info.height as usize);
    f.render_widget(
        Paragraph::new(lines),
        Rect {
            height: 3.min(info.height),
            ..info
        },
    );

    let rate_rows = [
        (
            3u16,
            "read",
            &reads,
            p.read_bps,
            p.hint_read_bps,
            p.read_total,
            READ,
        ),
        (
            4,
            "write",
            &writes,
            p.write_bps,
            p.hint_write_bps,
            p.write_total,
            WRITE,
        ),
    ];
    for (dy, name, data, bps, bound, total, stops) in rate_rows {
        if dy >= info.height {
            break;
        }
        let row = Rect {
            y: info.y + dy,
            height: 1,
            ..info
        };
        let unattributed = bps < 1.0 && bound > 0.0 && p.idle_for.is_none();
        let (rate, style) = if unattributed {
            (
                format!("≤ {}", fmt_rate(bound)),
                Style::new().fg(DIM).add_modifier(Modifier::ITALIC),
            )
        } else {
            (rate_text(bps), Style::new().fg(heat(bps, stops)))
        };
        let text = format!(" {rate:>13}  Σ {}", fmt_bytes(total));
        let text_w = 30.min(row.width);
        let [lab, graph, num] = Layout::horizontal([
            Constraint::Length(7),
            Constraint::Fill(1),
            Constraint::Length(text_w),
        ])
        .areas(row);
        f.render_widget(Paragraph::new(Span::styled(format!("{name:<7}"), dim)), lab);
        f.render_widget(
            Graph {
                data,
                max: scale,
                grow: Grow::Up,
                stops,
            },
            graph,
        );
        f.render_widget(Paragraph::new(Span::styled(text, style)), num);
    }
    if info.height > 5 {
        let text = match p.io {
            Some(io) => format!(
                "all files  read {}  write {}   disk  read {}  write {}",
                rate_text(io.rchar_bps),
                rate_text(io.wchar_bps),
                rate_text(io.read_bytes_bps),
                rate_text(io.write_bytes_bps)
            ),
            None if p.idle_for.is_some() => {
                "no longer has anything open under the path".to_string()
            }
            None => "all files  process-wide counters not readable".to_string(),
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                truncate_right(&text, info.width as usize),
                dim,
            )),
            Rect {
                y: info.y + 5,
                height: 1,
                ..info
            },
        );
    }

    app.file_rows = Rect::default();
    if files.height < 2 {
        return;
    }
    if p.files.is_empty() {
        empty_message(f, files, "nothing open under the path right now");
        return;
    }

    let fixed = 8 + 5 + 11 + 14 + 4;
    let path_w = files.width.saturating_sub(fixed) as usize;
    let title = |s: &str| {
        Cell::from(Span::styled(
            s.to_string(),
            Style::new().fg(TITLE).add_modifier(Modifier::BOLD),
        ))
    };
    let header = Row::new(vec![
        title("fd"),
        title("mode"),
        title("rate"),
        title("progress"),
        title("path"),
    ]);
    let rows: Vec<Row> = p
        .files
        .iter()
        .map(|of| file_row(of, &app.target, path_w))
        .collect();
    let highlight = if app.focus == Focus::Files {
        SELECTED
    } else {
        Color::Reset
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Length(11),
            Constraint::Length(14),
            Constraint::Fill(1),
        ],
    )
    .header(header)
    .column_spacing(1)
    .row_highlight_style(Style::new().bg(highlight).add_modifier(Modifier::BOLD));
    app.file_rows = Rect {
        y: files.y + 1,
        height: files.height - 1,
        ..files
    };
    f.render_stateful_widget(table, files, &mut app.file_state);
}

fn file_row(of: &OpenFile, target: &crate::target::Target, path_w: usize) -> Row<'static> {
    let dim = Style::new().fg(DIM);
    let fds = if of.fds.is_empty() {
        "-".to_string()
    } else {
        truncate_right(
            &of.fds
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
            8,
        )
    };
    let mode = match (of.kind, of.access) {
        (Kind::Dir, _) => "dir",
        (Kind::Map, _) => "map",
        (Kind::Other, _) => "other",
        (Kind::File, Access::Read) => "R",
        (Kind::File, Access::Write) => "W",
        (Kind::File, Access::ReadWrite) => "RW",
    };
    let (rate, stops) = if of.write_bps > of.read_bps {
        (of.bps(), WRITE)
    } else {
        (of.bps(), READ)
    };

    let progress = if of.kind == Kind::File && of.size > 0 {
        let frac = (of.pos as f64 / of.size as f64).clamp(0.0, 1.0);
        let filled = (frac * 8.0).round() as usize;
        Line::from(vec![
            Span::styled("▰".repeat(filled), Style::new().fg(gradient(stops, frac))),
            Span::styled("▱".repeat(8 - filled), Style::new().fg(FAINT)),
            Span::styled(format!(" {:>3.0}%", frac * 100.0), dim),
        ])
    } else {
        Line::from(Span::styled("-", dim))
    };

    let mut prefix = Vec::new();
    if of.deleted {
        prefix.push(Span::styled("✗ ", Style::new().fg(WARN)));
    }
    if of.shared {
        prefix.push(Span::styled("⇄ ", dim));
    }
    let used: usize = prefix.iter().map(|s| s.content.chars().count()).sum();
    let mut path = prefix;
    path.push(Span::styled(
        truncate_left(&target.rel(&of.path), path_w.saturating_sub(used)),
        Style::new().fg(if of.kind == Kind::Dir { DIM } else { TEXT }),
    ));

    Row::new(vec![
        Cell::from(fds).style(dim),
        Cell::from(mode).style(Style::new().fg(if of.kind == Kind::File { TEXT } else { DIM })),
        Cell::from(
            Line::from(Span::styled(
                rate_text(rate),
                Style::new().fg(heat(rate, stops)),
            ))
            .right_aligned(),
        ),
        Cell::from(progress),
        Cell::from(Line::from(path)),
    ])
}

// --- footer and help ----------------------------------------------------------

fn draw_read_history(f: &mut Frame, app: &mut App, area: Rect) {
    app.file_rows = Rect::default();
    let [heading, body] = Layout::vertical([
        Constraint::Length(if app.snap.as_ref().is_some_and(|s| s.warning.is_some()) {
            4
        } else {
            3
        }),
        Constraint::Fill(1),
    ])
    .areas(area);
    let counters = if cfg!(windows) {
        "Windows read transfer bytes (files, network and devices)"
    } else {
        "Linux rchar (read syscalls); disk bytes shown separately"
    };
    let status = app.snap.as_ref().map_or_else(
        || "scanning...".into(),
        |s| {
            format!(
                "{} observed / {} scanned | {} unreadable | {} ms refresh{}",
                s.totals.procs,
                s.scanned,
                s.denied,
                app.interval.as_millis(),
                if app.paused { " | PAUSED" } else { "" }
            )
        },
    );
    let mut heading_lines = vec![
        Line::styled(
            " per-process read I/O history",
            Style::new().fg(TITLE).bold(),
        ),
        Line::styled(format!(" {counters}"), Style::new().fg(DIM)),
        Line::styled(format!(" {status}"), Style::new().fg(DIM)),
    ];
    if let Some(warning) = app.snap.as_ref().and_then(|s| s.warning.as_ref()) {
        heading_lines.push(Line::styled(warning.clone(), Style::new().fg(WARN)));
    }
    f.render_widget(Paragraph::new(heading_lines), heading);
    let (list, detail) = if area.width >= 110 {
        let [a, b] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Fill(1)]).areas(body);
        (a, b)
    } else {
        let [a, b] =
            Layout::vertical([Constraint::Percentage(40), Constraint::Fill(1)]).areas(body);
        (a, b)
    };
    let block = panel("processes | observed read total", BORDER_PROCS);
    let inner = block.inner(list);
    f.render_widget(block, list);
    let rows: Vec<Row> = app
        .snap
        .as_ref()
        .map(|s| {
            app.view
                .iter()
                .map(|&i| {
                    let p = &s.procs[i];
                    Row::new(vec![
                        Cell::from(p.pid.to_string()),
                        Cell::from(p.comm.clone()),
                        rate_cell(p.read_bps, READ, p.io.is_none()),
                        Cell::from(fmt_bytes(p.read_total)),
                        Cell::from(if p.io.is_some() { "live" } else { "unseen" }),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    app.proc_rows = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    f.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(7),
                Constraint::Fill(1),
                Constraint::Length(12),
                Constraint::Length(11),
                Constraint::Length(6),
            ],
        )
        .header(
            Row::new(["PID", "PROCESS", "READ/s", "TOTAL", "STATE"])
                .style(Style::new().fg(TITLE).bold()),
        )
        .column_spacing(1)
        .row_highlight_style(Style::new().bg(SELECTED).bold()),
        inner,
        &mut app.proc_state,
    );
    if app.view.is_empty() {
        empty_message(
            f,
            inner,
            if app.snap.is_none() {
                "scanning..."
            } else {
                "no observable processes match this view"
            },
        );
    }
    let Some(p) = app.selected_proc().cloned() else {
        f.render_widget(
            Paragraph::new("Select a process to inspect its read history")
                .block(panel("read history", BORDER_DETAIL)),
            detail,
        );
        return;
    };
    let block = panel(
        &format!("pid {} | {} | read history", p.pid, p.comm),
        BORDER_DETAIL,
    );
    let inner = block.inner(detail);
    f.render_widget(block, detail);
    let graph_height = if inner.height >= 14 {
        (inner.height / 4).clamp(3, 8)
    } else {
        1
    };
    let samples_height = if inner.height >= 16 { 4 } else { 1 };
    let [stats, graph, samples, files] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(graph_height),
        Constraint::Length(samples_height),
        Constraint::Fill(1),
    ])
    .areas(inner);
    let history = app.read_history.get(&p.pid);
    let peak = history.map_or(0.0, |h| h.iter().map(|s| s.bps).fold(0.0, f64::max));
    f.render_widget(
        Paragraph::new(vec![
            Line::styled(
                truncate_right(&p.cmdline, stats.width as usize),
                Style::new().fg(DIM),
            ),
            Line::styled(
                format!(
                    "read {} | peak {} | observed {}",
                    fmt_rate(p.read_bps),
                    fmt_rate(peak),
                    fmt_bytes(p.read_total)
                ),
                Style::new().fg(TEXT),
            ),
            Line::styled(
                if p.io.is_some() {
                    "240 samples; totals count bytes observed since monitoring began"
                } else {
                    "Process no longer observable; recent samples retained for 2 minutes"
                },
                Style::new().fg(DIM),
            ),
        ]),
        stats,
    );
    let series: Vec<_> = history
        .map(|h| h.iter().map(|s| s.bps).collect())
        .unwrap_or_default();
    f.render_widget(
        Graph {
            data: &series,
            max: peak.max(1024.0),
            grow: Grow::Up,
            stops: READ,
        },
        graph,
    );
    let now = std::time::Instant::now();
    let rows: Vec<_> = history
        .map(|h| {
            h.iter()
                .rev()
                .take(samples.height.saturating_sub(1) as usize)
                .map(|s| {
                    Row::new(vec![
                        Cell::from(format!(
                            "{:.1}s ago",
                            now.duration_since(s.at).as_secs_f64()
                        )),
                        Cell::from(if s.observed {
                            fmt_rate(s.bps)
                        } else {
                            "unseen".into()
                        }),
                        Cell::from(s.disk_bps.map_or_else(|| "n/a".into(), fmt_rate)),
                        Cell::from(fmt_bytes(s.total)),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    f.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(12),
                Constraint::Fill(1),
                Constraint::Fill(1),
                Constraint::Fill(1),
            ],
        )
        .column_spacing(1)
        .header(
            Row::new(["SAMPLE AGE", "READ/s", "DISK READ/s", "TOTAL"]).style(Style::new().fg(DIM)),
        ),
        samples,
    );
    draw_file_history(f, app, p.pid, files);
}

fn draw_file_history(f: &mut Frame, app: &mut App, pid: u32, area: Rect) {
    let block = panel(
        "file access history | Tab to select",
        if app.focus == Focus::Files {
            HOTKEY
        } else {
            BORDER_DETAIL
        },
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(visits) = app.file_history.get(&pid).filter(|v| !v.is_empty()) else {
        empty_message(
            f,
            inner,
            "No file handles observed yet (brief accesses may be missed)",
        );
        app.file_rows = Rect::default();
        return;
    };
    let now = std::time::Instant::now();
    let detail_height = if inner.height >= 5 { 2 } else { 0 };
    let [table_area, info] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(detail_height)]).areas(inner);
    let path_width = table_area.width.saturating_sub(33) as usize;
    let rows: Vec<_> = visits
        .iter()
        .map(|v| {
            let state = match v.open {
                Some(true) => "open",
                Some(false) => "closed",
                None => "unseen",
            };
            let mode = match v.access {
                Access::Read => "R",
                Access::Write => "W",
                Access::ReadWrite => "RW",
            };
            let kind = match v.kind {
                Kind::File => "",
                Kind::Dir => " [dir]",
                Kind::Map => " [map]",
                Kind::Other => " [other]",
            };
            let path = format!(
                "{}{kind}{}{}",
                v.path.display(),
                if v.deleted { " [deleted]" } else { "" },
                if v.shared { " [shared]" } else { "" }
            );
            Row::new(vec![
                Cell::from(state),
                Cell::from(mode),
                Cell::from(format!(
                    "{:.1}s",
                    now.duration_since(v.last_seen).as_secs_f64()
                )),
                rate_cell(v.read_bps, READ, v.open != Some(true)),
                Cell::from(truncate_left(&path, path_width)),
            ])
            .style(Style::new().fg(if v.open == Some(true) { TEXT } else { DIM }))
        })
        .collect();
    app.file_rows = Rect {
        y: table_area.y + 1,
        height: table_area.height.saturating_sub(1),
        ..table_area
    };
    f.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(6),
                Constraint::Length(3),
                Constraint::Length(8),
                Constraint::Length(11),
                Constraint::Fill(1),
            ],
        )
        .column_spacing(1)
        .header(
            Row::new(["STATE", "MODE", "LAST SEEN", "READ/s est", "FILE / HANDLE"])
                .style(Style::new().fg(TITLE)),
        )
        .row_highlight_style(Style::new().bg(if app.focus == Focus::Files {
            SELECTED
        } else {
            SELECTED_DIM
        })),
        table_area,
        &mut app.file_state,
    );
    if let Some(v) = app.file_state.selected().and_then(|i| visits.get(i)) {
        f.render_widget(Paragraph::new(vec![
            Line::styled(v.path.display().to_string(), Style::new().fg(TEXT)),
            Line::styled(format!("first {:.1}s ago | last {:.1}s ago | {} observations | offset {} / {} | write est {}", now.duration_since(v.first_seen).as_secs_f64(), now.duration_since(v.last_seen).as_secs_f64(), v.visits, fmt_bytes(v.pos), fmt_bytes(v.size), fmt_rate(v.write_bps)), Style::new().fg(DIM)),
        ]), info);
    }
}

fn draw_file_details(f: &mut Frame, app: &App, area: Rect) {
    let Some(visit) = app
        .selected
        .and_then(|pid| app.file_history.get(&pid))
        .and_then(|files| app.file_state.selected().and_then(|i| files.get(i)))
    else {
        return;
    };
    let width = area.width.saturating_sub(4).min(110);
    let height = area.height.saturating_sub(2).min(18);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let now = std::time::Instant::now();
    let age = |time: Option<std::time::Instant>| {
        time.map_or_else(
            || "not observed".into(),
            |t| format!("{:.1}s ago", now.duration_since(t).as_secs_f64()),
        )
    };
    let access = match visit.access {
        Access::Read => "read",
        Access::Write => "write",
        Access::ReadWrite => "read/write",
    };
    let state = match visit.open {
        Some(true) => "open",
        Some(false) => "closed",
        None => "unseen (handles unavailable)",
    };
    let lines = vec![
        Line::styled(
            visit.path.display().to_string(),
            Style::new().fg(TITLE).bold(),
        ),
        Line::raw(""),
        Line::raw(format!(
            "{state} | access {access} | {:?} | {} observation periods",
            visit.kind, visit.visits
        )),
        Line::raw(format!(
            "first seen {} | last seen {}",
            age(Some(visit.first_seen)),
            age(Some(visit.last_seen))
        )),
        Line::raw(format!(
            "last estimated read {} | write {}",
            age(visit.last_read),
            age(visit.last_write)
        )),
        Line::raw(format!(
            "offset {} | size {}",
            fmt_bytes(visit.pos),
            fmt_bytes(visit.size)
        )),
        Line::raw(format!(
            "estimated read {} | write {}",
            fmt_rate(visit.read_bps),
            fmt_rate(visit.write_bps)
        )),
        Line::raw(format!(
            "deleted {} | shared handle {}",
            visit.deleted, visit.shared
        )),
        Line::raw(""),
        Line::styled(
            "Sampled handles and offset estimates; short-lived accesses can be missed.",
            Style::new().fg(DIM),
        ),
        Line::styled("Press any key to close", Style::new().fg(DIM)),
    ];
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel("file details", HOTKEY)),
        rect,
    );
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    if app.editing_filter {
        let line = Line::from(vec![
            Span::styled(" / ", Style::new().fg(HOTKEY).add_modifier(Modifier::BOLD)),
            Span::styled(app.filter.clone(), Style::new().fg(TITLE)),
            Span::styled("▏", Style::new().fg(TITLE)),
            Span::styled("   enter accept · esc clear", Style::new().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let hints: [(&str, String); 11] = [
        ("q", "uit".into()),
        ("m", " mode".into()),
        ("↑↓", " select".into()),
        ("s", format!("ort {}", app.sort.label())),
        ("r", "everse".into()),
        ("i", "dle".into()),
        ("p", "ause".into()),
        ("+-", " refresh".into()),
        ("g", "roup".into()),
        ("/", " filter".into()),
        ("?", " help".into()),
    ];
    let mut spans = vec![Span::raw(" ")];
    for (key, text) in hints {
        spans.push(Span::styled(
            key,
            Style::new().fg(HOTKEY).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!("{text}  "), Style::new().fg(DIM)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let w = 66.min(area.width);
    let h = 25.min(area.height);
    let rect = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    let key = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(
                format!(" {k:<14}"),
                Style::new().fg(HOTKEY).add_modifier(Modifier::BOLD),
            ),
            Span::styled(d.to_string(), Style::new().fg(TEXT)),
        ])
    };
    let note = |s: &str| Line::from(Span::styled(format!(" {s}"), Style::new().fg(DIM)));
    let lines = vec![
        key(
            "↑ ↓  j k",
            "select process (or file, when the list has focus)",
        ),
        key("PgUp PgDn", "move ten rows"),
        key("Home End", "first / last"),
        key("Tab", "switch focus between processes and open files"),
        key("← →  s", "change sort column"),
        key("r", "reverse sort order"),
        key("i", "show only processes that are moving data"),
        key("/", "filter by name, pid, user, command or path"),
        key("g", "group folders by depth below the target"),
        key("m", "switch path / per-process read history mode"),
        key("Enter", "inspect selected file in history mode"),
        key("+ -", "refresh interval"),
        key("p  space", "pause / resume"),
        key("R", "refresh now"),
        key("q  ctrl-c", "quit"),
        Line::raw(""),
        note("Path rates estimate file offsets; history uses process counters."),
        note("≤ N/s: the process moved up to N overall while no offset under"),
        note("the path advanced (pread, mmap, or its other files/sockets)."),
        note("Opens shorter than the refresh interval can be missed. Other"),
        note("users' processes may need root/Administrator privileges."),
        Line::raw(""),
        Line::from(Span::styled(
            " press any key to close",
            Style::new().fg(DIM),
        )),
    ];
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(panel("help", HOTKEY)), rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{FolderActivity, ProcActivity, ProcIo, Snapshot, Totals};
    use crate::target::Target;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    fn file(
        path: &str,
        kind: Kind,
        access: Access,
        read: f64,
        write: f64,
        pos: u64,
        size: u64,
    ) -> OpenFile {
        OpenFile {
            fds: vec![7],
            path: path.into(),
            kind,
            access,
            pos,
            size,
            read_bps: read,
            write_bps: write,
            deleted: false,
            shared: false,
        }
    }

    fn demo() -> App {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            Target::new(PathBuf::from("/tmp")),
            Duration::from_millis(500),
            tx,
        );
        let mut flac = file(
            "/tmp/music/album/track01.flac",
            Kind::File,
            Access::Read,
            48.0e6,
            0.0,
            30 << 20,
            50 << 20,
        );
        flac.fds = vec![11, 12];
        let mut gone = file(
            "/tmp/music/album/old.tmp",
            Kind::File,
            Access::ReadWrite,
            0.0,
            0.0,
            10,
            100,
        );
        gone.deleted = true;
        gone.shared = true;
        let mpv = ProcActivity {
            pid: 4242,
            identity: 4242,
            ppid: 1,
            comm: "mpv".into(),
            cmdline: "mpv --no-video /tmp/music/album/track01.flac".into(),
            user: "gec".into(),
            state: 'S',
            cwd: Some("/tmp/music".into()),
            files: vec![
                flac,
                gone,
                file("/tmp/music/album", Kind::Dir, Access::Read, 0.0, 0.0, 0, 0),
                file(
                    "/tmp/music/cache.db",
                    Kind::Map,
                    Access::Read,
                    0.0,
                    0.0,
                    0,
                    4096,
                ),
            ],
            files_observed: true,
            read_bps: 48.0e6,
            write_bps: 0.0,
            read_total: 3 << 30,
            write_total: 0,
            io: Some(ProcIo {
                rchar_bps: 50.0e6,
                wchar_bps: 1000.0,
                read_bytes_bps: 0.0,
                write_bytes_bps: 4096.0,
            }),
            hint_read_bps: 0.0,
            hint_write_bps: 0.0,
            focus: Some("/tmp/music/album".into()),
            idle_for: None,
        };
        let rsync = ProcActivity {
            pid: 99,
            comm: "rsync".into(),
            cmdline: "rsync -a /tmp/music/ /mnt/backup/".into(),
            files: vec![file(
                "/tmp/music/b/very/deep/folder/name/that/is/long/file.wav",
                Kind::File,
                Access::Write,
                0.0,
                3.0e6,
                5,
                10,
            )],
            read_bps: 0.0,
            write_bps: 3.0e6,
            focus: Some("/tmp/music/b/very/deep/folder/name/that/is/long".into()),
            ..mpv.clone()
        };
        let indexer = ProcActivity {
            pid: 55,
            comm: "indexer".into(),
            cmdline: "indexer --db /tmp/music/lib.db".into(),
            files: vec![file(
                "/tmp/music/lib.db",
                Kind::File,
                Access::ReadWrite,
                0.0,
                0.0,
                0,
                1 << 30,
            )],
            read_bps: 0.0,
            write_bps: 0.0,
            io: Some(ProcIo {
                rchar_bps: 1.7e9,
                wchar_bps: 0.0,
                read_bytes_bps: 9.0e7,
                write_bytes_bps: 0.0,
            }),
            hint_read_bps: 1.7e9,
            focus: Some("/tmp/music".into()),
            ..mpv.clone()
        };
        let idle = ProcActivity {
            pid: 7,
            comm: "bash".into(),
            files: vec![],
            read_bps: 0.0,
            write_bps: 0.0,
            io: None,
            focus: Some("/tmp/music".into()),
            idle_for: Some(Duration::from_secs(2)),
            ..mpv.clone()
        };
        let folder = |p: &str, r, w, pids: Vec<u32>| FolderActivity {
            path: p.into(),
            read_bps: r,
            write_bps: w,
            pids,
            files: 1,
            opened: 0,
        };
        for i in 0..30 {
            app.history.push_back((
                20.0e6 * (i as f64 / 6.0).sin().abs() + 1e6,
                1e6 * (i % 5) as f64,
            ));
        }
        app.proc_history.insert(
            4242,
            (0..30).map(|i| (30.0e6 + i as f64 * 1e6, 0.0)).collect(),
        );
        app.on_snapshot(Snapshot {
            procs: vec![mpv, rsync, indexer, idle],
            folders: vec![
                folder("/tmp/music/album", 48.0e6, 0.0, vec![4242]),
                folder(
                    "/tmp/music/b/very/deep/folder/name/that/is/long",
                    0.0,
                    3.0e6,
                    vec![99],
                ),
                folder("/tmp/music", 0.0, 0.0, vec![7, 4242]),
            ],
            totals: Totals {
                read_bps: 48.0e6,
                write_bps: 3.0e6,
                read_total: 5 << 30,
                write_total: 300 << 20,
                procs: 2,
                files: 3,
                dirs: 1,
                maps: 1,
            },
            scanned: 312,
            denied: 41,
            scan_time: Duration::from_millis(7),
            mode: Mode::Path,
            warning: None,
        });
        app
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        let text = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        if std::env::var_os("WHOUSE_DUMP").is_some() {
            println!("{text}");
        }
        text
    }

    #[test]
    fn wide_layout_shows_every_panel() {
        let mut app = demo();
        let s = render(&mut app, 140, 40);
        for want in [
            "throughput",
            "target",
            "processes",
            "folders",
            "pid 4242 · mpv",
            "mpv",
            "rsync",
            "bash",
            "45.8 MiB/s",
            "▲ read",
            "▼ write",
            "scale",
            "track01.flac",
            "./music/album",
            "11,12",
            "dir",
            "map",
            "✗ ⇄",
            "41 unreadable processes",
            "5.00 GiB",
            "g: full depth",
            "all files",
            "312 procs",
            "uit",
            "indexer",
            "≤ 1.58 GiB/s",
        ] {
            assert!(s.contains(want), "missing {want:?} in:\n{s}");
        }
        assert!(s.contains('●'), "selected process marker in folders panel");
    }

    #[test]
    fn narrow_layout_drops_folders_panel_but_stays_usable() {
        let mut app = demo();
        let s = render(&mut app, 80, 30);
        assert!(s.contains("processes") && s.contains("pid 4242"), "{s}");
        assert!(!s.contains("folders"), "{s}");
    }

    #[test]
    fn read_history_mode_shows_process_counters_and_timestamped_samples() {
        let mut app = demo();
        let mut snap = app.snap.clone().unwrap();
        app.on_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Char('m'),
        ));
        snap.mode = Mode::ReadHistory;
        app.on_snapshot(snap);
        for (w, h) in [(140, 40), (80, 30), (70, 20)] {
            let text = render(&mut app, w, h);
            assert!(text.contains("per-process read I/O history"), "{text}");
            assert!(text.contains("SAMPLE AGE"), "{text}");
            assert!(text.contains("DISK READ/s"), "{text}");
            assert!(!text.contains("folders"), "{text}");
        }
        let text = render(&mut app, 140, 40);
        assert!(
            text.contains("track01.flac") && text.contains("FILE / HANDLE"),
            "{text}"
        );
        let index = app.file_history[&4242]
            .iter()
            .position(|f| f.path.ends_with("track01.flac"))
            .unwrap();
        app.file_state.select(Some(index));
        app.on_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Tab,
        ));
        app.on_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Enter,
        ));
        let text = render(&mut app, 140, 40);
        assert!(
            text.contains("file details") && text.contains("/tmp/music/album/track01.flac"),
            "{text}"
        );
        assert!(text.contains("last estimated read"), "{text}");
    }

    #[test]
    fn tiny_terminal_and_empty_states_do_not_panic() {
        let mut app = demo();
        assert!(render(&mut app, 50, 12).contains("terminal too small"));
        let (tx, _rx) = mpsc::channel();
        let mut empty = App::new(
            Target::new(PathBuf::from("/tmp")),
            Duration::from_millis(500),
            tx,
        );
        assert!(render(&mut empty, 120, 36).contains("scanning"));
        empty.on_snapshot(Snapshot {
            procs: vec![],
            folders: vec![],
            totals: Totals::default(),
            scanned: 1,
            denied: 0,
            scan_time: Duration::ZERO,
            mode: Mode::Path,
            warning: None,
        });
        assert!(render(&mut empty, 120, 36).contains("nothing has /tmp open right now"));
        for (w, h) in [(70, 20), (71, 21), (109, 23), (110, 24), (300, 80)] {
            render(&mut app, w, h);
        }
        app.show_help = true;
        assert!(render(&mut app, 120, 36).contains("press any key"));
    }

    #[test]
    fn mouse_clicks_select_rows_and_the_wheel_scrolls() {
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut app = demo();
        render(&mut app, 140, 40);
        let ev = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let rows = app.proc_rows;
        assert_eq!(app.selected, Some(4242));

        app.on_mouse(ev(
            MouseEventKind::Down(MouseButton::Left),
            rows.x + 3,
            rows.y + 1,
        ));
        assert_eq!(app.selected, Some(99), "second row");
        app.on_mouse(ev(MouseEventKind::ScrollDown, rows.x + 3, rows.y));
        assert_eq!(app.selected, Some(7), "wheel clamps at the last row");
        app.on_mouse(ev(
            MouseEventKind::Down(MouseButton::Left),
            rows.x + 3,
            rows.y + 50,
        ));
        assert_eq!(
            app.selected,
            Some(7),
            "a click below the last row does nothing"
        );

        render(&mut app, 140, 40);
        app.on_mouse(ev(
            MouseEventKind::Down(MouseButton::Left),
            rows.x + 3,
            rows.y,
        ));
        assert_eq!(app.selected, Some(4242));
        render(&mut app, 140, 40);
        let files = app.file_rows;
        app.on_mouse(ev(
            MouseEventKind::Down(MouseButton::Left),
            files.x + 3,
            files.y + 2,
        ));
        assert_eq!(app.focus, Focus::Files);
        assert_eq!(app.file_state.selected(), Some(2));
        app.on_mouse(ev(MouseEventKind::ScrollUp, files.x + 3, files.y));
        assert_eq!(app.file_state.selected(), Some(0));
    }

    #[test]
    fn records_row_areas_for_mouse_hits_and_scrolls_long_lists() {
        let mut app = demo();
        render(&mut app, 140, 40);
        assert!(app.proc_rows.height > 3 && app.file_rows.height > 3);
        let snap = app.snap.as_ref().unwrap().clone();
        let many: Vec<ProcActivity> = (0..80)
            .map(|i| ProcActivity {
                pid: 1000 + i,
                ..snap.procs[0].clone()
            })
            .collect();
        app.on_snapshot(Snapshot {
            procs: many,
            ..snap
        });
        for _ in 0..60 {
            app.on_key(ratatui::crossterm::event::KeyEvent::from(
                ratatui::crossterm::event::KeyCode::Down,
            ));
        }
        let s = render(&mut app, 140, 40);
        assert!(s.contains("1060"), "selected row scrolled into view:\n{s}");
        assert!(s.contains("80/80"));
    }
}
