mod app;
mod graph;
mod scan;
mod target;
mod ui;
mod util;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use ratatui::crossterm::execute;

use app::App;
use scan::{Options, Scanner, Snapshot};
use target::Target;

#[derive(Parser)]
#[command(
    version,
    about = "See which processes are reading, writing and opening a path"
)]
struct Cli {
    /// File or folder to watch
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Refresh interval in milliseconds
    #[arg(short, long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(50..=10_000))]
    interval: u64,

    /// Skip /proc/<pid>/maps, so memory-mapped files are not reported (cheaper on busy machines)
    #[arg(long)]
    no_maps: bool,
}

enum Msg {
    Input(Event),
    Snapshot(Box<Snapshot>),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = std::fs::canonicalize(&cli.path)
        .with_context(|| format!("cannot open {}", cli.path.display()))?;
    if !std::path::Path::new("/proc/self/fd").is_dir() {
        bail!("/proc is not mounted: whouse needs it to see open files");
    }
    let interval = Duration::from_millis(cli.interval);

    let (tx, rx) = mpsc::channel::<Msg>();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let scan_tx = tx.clone();
    let scanner = Scanner::new(
        root.clone(),
        Options {
            maps: !cli.no_maps,
            ..Options::default()
        },
    );
    scan::spawn(scanner, interval, cmd_rx, move |snap| {
        scan_tx.send(Msg::Snapshot(Box::new(snap))).is_ok()
    });
    thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx.send(Msg::Input(ev)).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(Target::new(root), interval, cmd_tx);

    // `ratatui::init` installs a panic hook that restores the terminal; add
    // mouse capture to what gets undone.
    let mut terminal = ratatui::try_init().context("whouse needs an interactive terminal")?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        hook(info);
    }));
    execute!(stdout(), EnableMouseCapture)?;
    let result = run(&mut terminal, &mut app, rx);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, rx: Receiver<Msg>) -> Result<()> {
    while !app.quit {
        terminal.draw(|f| ui::draw(f, app))?;

        // Block for one message, then drain whatever piled up so a slow draw
        // never lags behind input or stale snapshots.
        let Ok(first) = rx.recv() else { break };
        for msg in std::iter::once(first).chain(rx.try_iter()) {
            match msg {
                Msg::Snapshot(s) => app.on_snapshot(*s),
                Msg::Input(Event::Key(k)) => app.on_key(k),
                Msg::Input(Event::Mouse(m)) => app.on_mouse(m),
                Msg::Input(_) => {} // resize: the next draw picks up the new size
            }
        }
    }
    Ok(())
}
