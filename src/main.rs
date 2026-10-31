//! gpudash — live terminal dashboard for your Slurm jobs across clusters.

mod app;
mod background;
mod config;
mod highlight;
mod notifier;
mod palette;
mod remote;
mod theme;
mod ui;
mod util;

use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::execute;

use app::{App, Next, Popup};
use palette::{Act, Palette};

const HELP: &str = "\
gpudash — live dashboard for your Slurm jobs across clusters

usage: gpudash [-n SECONDS] [HOST ...]   (gpudash --version shows the build)
       gpudash background on|off|status

  HOST        ssh host names to watch (default: the clusters in the config file)
  -n SECONDS  how often to refresh (overrides the config for this run)

After the first run gpudash keeps sending notifications in the background (macOS),
polling every 5 minutes while the dashboard is closed. `gpudash background off` stops that.

config: ~/.config/gpudash/config.toml
  Press p inside gpudash to change settings and add or remove clusters.
  A minimal config:

    [[cluster]]
    host = \"explorer\"                  # ssh host name from ~/.ssh/config
    prefix = \"/projects/myname/\"       # optional: hidden in the folder column
";

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1).peekable();
    match args.peek().map(String::as_str) {
        Some("daemon") => background::run_daemon(),
        Some("background") => {
            args.next();
            return background_command(args.next().as_deref());
        }
        _ => {}
    }
    let mut interval = None;
    let mut hosts = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-V" | "--version" => {
                println!("gpudash {} ({})", env!("CARGO_PKG_VERSION"), env!("GPUDASH_COMMIT"));
                return Ok(());
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            "-n" => interval = args.next().and_then(|s| s.parse().ok()),
            _ => hosts.push(a),
        }
    }

    let (mut config, err) = config::load();
    if let Some(e) = err {
        eprintln!("gpudash: couldn't read the config file\n  {e}");
        std::process::exit(1);
    }
    if let Some(n) = interval {
        config.refresh_seconds = n;
    }
    if hosts.is_empty() && config.clusters.is_empty() {
        eprint!("No clusters to watch yet. Pass ssh host names (gpudash mycluster), or add them to the config.\n\n{HELP}");
        std::process::exit(1);
    }

    theme::write_examples();
    let theme_error = theme::apply(&config.theme).err();
    let mut app = App::new(config, hosts);
    app.theme_shown = app.config.theme.clone();
    if let Some(e) = theme_error {
        app.say(e, true);
    }
    let mut terminal = init_terminal();
    // if gpudash ever crashes, give the terminal its mouse back (ratatui's own hook does the rest)
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        previous(info);
    }));
    if app.config.background {
        match background::ensure() {
            Ok(true) => app.say("gpudash will keep sending notifications after you close it (p → Background to change)", false),
            Ok(false) => {}
            Err(e) => app.say(format!("background notifications: {e}"), true),
        }
    }
    background::heartbeat();
    app.refresh();
    let result = run(&mut terminal, &mut app);
    background::heartbeat_stop();
    restore_terminal();
    result
}

/// `gpudash background on|off|status`
fn background_command(what: Option<&str>) -> std::io::Result<()> {
    let (mut config, _) = config::load();
    match what {
        Some("on") => {
            config.background = true;
            let _ = config::save(&config);
            match background::ensure() {
                Ok(_) => println!("background notifications on"),
                Err(e) => println!("couldn't turn on background notifications: {e}"),
            }
        }
        Some("off") => {
            config.background = false;
            let _ = config::save(&config);
            background::remove();
            println!("background notifications off");
        }
        Some("status") | None => println!("{}", background::status_text()),
        Some(other) => println!("unknown option \"{other}\": use on, off or status"),
    }
    Ok(())
}

/// Full-screen mode with mouse capture, so scroll-wheel events arrive as scrolling
/// rather than as arrow keys the terminal makes up.
fn init_terminal() -> DefaultTerminal {
    let terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    terminal
}

fn restore_terminal() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
}

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    loop {
        app.receive();
        let mut targets = ui::MouseTargets::default();
        terminal.draw(|f| targets = ui::draw(f, app))?;
        // handle everything that's waiting, so fast scrolling or typing never lags behind
        let mut wait = Duration::from_millis(80);
        let mut stale = false;
        while event::poll(wait)? {
            wait = Duration::ZERO;
            let ev = event::read()?;
            // an earlier event may have changed the layout, so redraw before hit-testing a click
            if stale && matches!(&ev, Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left)) {
                terminal.draw(|f| targets = ui::draw(f, app))?;
            }
            stale = true;
            let next = match ev {
                Event::Key(k) if k.kind == KeyEventKind::Press => on_key(app, k),
                Event::Mouse(m) => on_mouse(app, m, &targets),
                _ => Next::Continue,
            };
            match next {
                Next::Continue => {}
                Next::Quit => return Ok(()),
                Next::Shell { host, workdir, running_id } => {
                    suspend(terminal, || {
                        println!("\n  gpudash → {host}:{workdir}  (exit the shell to come back)\n");
                        remote::shell(&host, &workdir, running_id.as_deref())
                    })?;
                }
                Next::EditConfig => {
                    let Some(path) = config::path() else { continue };
                    if !path.exists() {
                        let _ = config::save(&app.config);
                    }
                    let editor = std::env::var("VISUAL")
                        .or_else(|_| std::env::var("EDITOR"))
                        .unwrap_or_else(|_| "vi".into());
                    suspend(terminal, || {
                        // $EDITOR may carry flags, e.g. "code --wait"
                        std::process::Command::new("sh")
                            .arg("-c")
                            .arg(format!("{editor} \"$1\""))
                            .arg("sh")
                            .arg(&path)
                            .status()
                            .map(|_| ())
                    })?;
                    app.reload_config();
                }
            }
}
        if app.tick % 100 == 0 {
            // about every 8s: tell the background daemon the dashboard is open
            background::heartbeat();
        }
        if !app.paused {
            app.auto_refresh();
        }
        app.tick += 1;
    }
}

/// Leave the full-screen view, run something in the normal terminal, then come back.
fn suspend(terminal: &mut DefaultTerminal, f: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
    restore_terminal();
    let result = f();
    *terminal = init_terminal();
    terminal.clear()?;
    if let Err(e) = result {
        eprintln!("{e}");
    }
    Ok(())
}

/// Lines the log moves per scroll-wheel tick.
const WHEEL_LINES: isize = 3;
/// A trackpad keeps sending scroll events for a moment after a flick. Scroll events that
/// arrive within this long of closing the log (or of the previous ignored one) are dropped,
/// so they don't move the dashboard selection.
const MOMENTUM: Duration = Duration::from_millis(400);

fn on_mouse(app: &mut App, m: MouseEvent, targets: &ui::MouseTargets) -> Next {
    if m.kind == MouseEventKind::Down(MouseButton::Left) {
        match targets.at(m.column, m.row) {
            Some(ui::MouseTarget::Job(ci, id)) => app.select(*ci, id.clone()),
            Some(ui::MouseTarget::Palette(index)) => {
                if let Some(level) = app.palette.as_mut().and_then(|p| p.level()) {
                    level.sel = *index;
                    return palette_key(app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                }
            }
            _ => {}
        }
        return Next::Continue;
    }
    let delta: isize = match m.kind {
        MouseEventKind::ScrollUp => -1,
        MouseEventKind::ScrollDown => 1,
        _ => return Next::Continue,
    };
    if app.palette.is_some() {
        // in the palette a wheel tick works like ↑ / ↓ (including the theme preview)
        let code = if delta < 0 { KeyCode::Up } else { KeyCode::Down };
        return palette_key(app, KeyEvent::new(code, KeyModifiers::NONE));
    }
    if app.popup.is_some() || app.filtering {
        return Next::Continue;
    }
    if app.log.is_some() {
        scroll_log(app, delta * WHEEL_LINES);
        return Next::Continue;
    }
    if let Some(closed) = app.log_closed {
        if closed.elapsed() < MOMENTUM {
            app.log_closed = Some(Instant::now());
            return Next::Continue;
        }
        app.log_closed = None;
    }
    app.step(delta);
    Next::Continue
}

/// Move the log view by `delta` lines (negative = up). Reaching the end resumes following.
fn scroll_log(app: &mut App, delta: isize) {
    let Some(v) = app.log.as_mut() else { return };
    let n = v.log.as_ref().map_or(0, |l| l.lines.len());
    let height = ratatui::crossterm::terminal::size().map_or(30, |(_, h)| h as usize).saturating_sub(2);
    let bottom = n.saturating_sub(height);
    let top = v.top.unwrap_or(bottom) as isize;
    let to = (top + delta).max(0) as usize;
    v.top = if to >= bottom { None } else { Some(to) };
}

fn on_key(app: &mut App, k: KeyEvent) -> Next {
    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        return Next::Quit;
    }

    if app.palette.is_some() {
        return palette_key(app, k);
    }

    // popups take the key first
    if let Some(popup) = app.popup.take() {
        if let Popup::ConfirmCancel { ci, id, .. } = popup {
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                app.cancel(ci, id);
            }
        }
        return Next::Continue;
    }

    if app.filtering {
        match k.code {
            KeyCode::Esc => {
                app.filter.clear();
                app.filtering = false;
            }
            KeyCode::Enter => app.filtering = false,
            KeyCode::Backspace => {
                app.filter.pop();
            }
            KeyCode::Down => app.step(1),
            KeyCode::Up => app.step(-1),
            KeyCode::Char(c) => app.filter.push(c),
            _ => {}
        }
        app.fix_selection();
        return Next::Continue;
    }

    if let Some(v) = app.log.as_mut() {
        let page = 20;
        let n = v.log.as_ref().map_or(0, |l| l.lines.len());
        let height = ratatui::crossterm::terminal::size().map_or(30, |(_, h)| h as usize).saturating_sub(2);
        let bottom = n.saturating_sub(height);
        let top = v.top.unwrap_or(bottom);
        let mut scroll = |to: usize| v.top = if to >= bottom { None } else { Some(to) };
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => {
                app.log = None;
                app.log_closed = Some(Instant::now());
            }
            KeyCode::Up | KeyCode::Char('k') => scroll(top.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => scroll(top + 1),
            KeyCode::PageUp | KeyCode::Char('u') => scroll(top.saturating_sub(page)),
            KeyCode::PageDown | KeyCode::Char('d') | KeyCode::Char(' ') => scroll(top + page),
            KeyCode::Home | KeyCode::Char('g') => scroll(0),
            KeyCode::End | KeyCode::Char('G') => scroll(bottom),
            KeyCode::Char('e') => app.toggle_log_stream(),
            KeyCode::Char('r') => app.fetch_log(),
            _ => {}
        }
        return Next::Continue;
    }

    match k.code {
        KeyCode::Char('p') | KeyCode::Char(':') => app.palette = Some(Palette::new()),
        KeyCode::Char('q') => return Next::Quit,
        KeyCode::Esc if !app.filter.is_empty() => {
            app.filter.clear();
            app.fix_selection();
        }
        KeyCode::Esc => return Next::Quit,
        KeyCode::Char('r') => app.refresh(),
        KeyCode::Down | KeyCode::Char('j') => app.step(1),
        KeyCode::Up | KeyCode::Char('k') => app.step(-1),
        KeyCode::PageDown => app.step(10),
        KeyCode::PageUp => app.step(-10),
        KeyCode::Home | KeyCode::Char('g') => app.jump(false),
        KeyCode::End | KeyCode::Char('G') => app.jump(true),
        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => app.open_log(),
        KeyCode::Char('x') => app.ask_cancel(),
        KeyCode::Char('y') => app.copy_id(),
        KeyCode::Char('/') => app.filtering = true,
        KeyCode::Char('?') => app.popup = Some(Popup::Help),
        KeyCode::Char('e') => app.cycle_ended(),
        KeyCode::Char('n') => return app.run(palette::Cmd::ToggleNotify),
        KeyCode::Char('s') => return app.shell_target(),
        _ => {}
    }
    Next::Continue
}

fn palette_key(app: &mut App, k: KeyEvent) -> Next {
    let next = palette_key_inner(app, k);
    preview_theme(app);
    next
}

/// While the Theme list is open, show the highlighted theme; otherwise show the saved one.
fn preview_theme(app: &mut App) {
    let highlighted = app.palette.as_ref().and_then(|p| p.stack.last()).and_then(|l| {
        if l.menu != palette::Menu::Theme {
            return None;
        }
        let items = palette::search(app, l.menu, &l.query);
        let last = items.len().checked_sub(1)?;
        match items.into_iter().nth(l.sel.min(last))?.act {
            Act::Run(palette::Cmd::Theme(name)) => Some(name),
            _ => None,
        }
    });
    let want = highlighted.unwrap_or_else(|| app.config.theme.clone());
    if want != app.theme_shown && theme::apply(&want).is_ok() {
        app.theme_shown = want;
    }
}

fn palette_key_inner(app: &mut App, k: KeyEvent) -> Next {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let p = app.palette.as_mut().unwrap();

    // text box (add cluster, set prefix)
    if let Some(prompt) = p.prompt.as_mut() {
        match k.code {
            KeyCode::Esc => app.palette = None,
            KeyCode::Enter => {
                let prompt = app.palette.take().unwrap().prompt.unwrap();
                app.submit_prompt(prompt);
            }
            KeyCode::Backspace => {
                prompt.text.pop();
            }
            KeyCode::Char(c) => prompt.text.push(c),
            _ => {}
        }
        return Next::Continue;
    }

    let Some(level) = p.level() else {
        app.palette = None;
        return Next::Continue;
    };
    let (menu, query, sel) = (level.menu, level.query.clone(), level.sel);
    let items: Vec<palette::Item> =
        palette::search(app, menu, &query);
    let last = items.len().saturating_sub(1);

    let p = app.palette.as_mut().unwrap();
    let depth = p.stack.len();
    let level = p.level().unwrap();
    match k.code {
        KeyCode::Esc => {
            p.stack.pop();
            if p.stack.is_empty() {
                app.palette = None;
            }
        }
        KeyCode::Up => level.sel = sel.saturating_sub(1),
        KeyCode::Down => level.sel = (sel + 1).min(last),
        KeyCode::Char('p') if ctrl => level.sel = sel.saturating_sub(1),
        KeyCode::Char('n') if ctrl => level.sel = (sel + 1).min(last),
        KeyCode::Backspace => {
            if level.query.pop().is_none() && depth > 1 {
                p.stack.pop();
            } else {
                level.sel = 0;
            }
        }
        KeyCode::Char(c) => {
            level.query.push(c);
            level.sel = 0;
        }
        KeyCode::Enter => {
            let Some(it) = items.into_iter().nth(sel.min(last)) else { return Next::Continue };
            match it.act {
                Act::Open(menu) => {
                    p.open(menu);
                    // start the theme list on the theme in use
                    if menu == palette::Menu::Theme {
                        let current = &app.config.theme;
                        let pos = theme::available().iter().position(|n| n == current).unwrap_or(0);
                        if let Some(l) = app.palette.as_mut().and_then(|p| p.level()) {
                            l.sel = pos;
                        }
                    }
                }
                Act::Run(cmd) => {
                    app.palette = None;
                    return app.run(cmd);
                }
            }
        }
        _ => {}
    }
    Next::Continue
}
