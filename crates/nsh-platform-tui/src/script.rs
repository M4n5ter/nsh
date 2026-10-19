//! Deterministic input and text snapshots without a real terminal. Steps
//! are separated by semicolons or newlines. Coordinates are terminal cells.
//! `wait`, `shot`, `tap`, `hover`, `wheel`, `key`, `type`, `selectall`,
//! `resize`, `dark`, `light`, `expect`, `expect-focus` and `quit` are checked
//! before the app starts, so a typo is an actionable failure.

use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::{AppView, Appearance};

pub(crate) struct Script(VecDeque<Step>);

enum Step {
    Wait(Duration),
    Shot(String),
    Tap(u16, u16),
    Hover(u16, u16),
    Wheel(u16, u16, i32, i32),
    Key(KeyEvent),
    Type(String),
    Resize(u16, u16),
    Appearance(Appearance),
    Expect(String),
    ExpectFocus,
    Quit,
}

impl Script {
    pub(crate) fn parse(script: &str) -> io::Result<Self> {
        let mut steps = VecDeque::new();
        for line in script
            .split([';', '\n'])
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let args: Vec<_> = rest.split_whitespace().collect();
            let error = || {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid script step: {line}"),
                )
            };
            let unsigned = |i: usize| {
                args.get(i)
                    .and_then(|arg| arg.parse::<u16>().ok())
                    .ok_or_else(error)
            };
            let signed = |i: usize| {
                args.get(i)
                    .and_then(|arg| arg.parse::<i32>().ok())
                    .ok_or_else(error)
            };
            let step = match command {
                "wait" if args.len() == 1 => {
                    Step::Wait(Duration::from_millis(args[0].parse().map_err(|_| error())?))
                }
                "shot" if !rest.trim().is_empty() => Step::Shot(rest.trim().into()),
                "tap" if args.len() == 2 => Step::Tap(unsigned(0)?, unsigned(1)?),
                "hover" if args.len() == 2 => Step::Hover(unsigned(0)?, unsigned(1)?),
                "wheel" if args.len() == 4 => {
                    Step::Wheel(unsigned(0)?, unsigned(1)?, signed(2)?, signed(3)?)
                }
                "key" if args.len() == 1 => Step::Key(key(args[0]).ok_or_else(error)?),
                "selectall" if args.is_empty() => {
                    Step::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
                }
                "type" => Step::Type(rest.to_owned()),
                "resize" if args.len() == 2 => {
                    let (columns, rows) = (unsigned(0)?, unsigned(1)?);
                    if columns == 0 || rows == 0 {
                        return Err(error());
                    }
                    Step::Resize(columns, rows)
                }
                "dark" if args.is_empty() => Step::Appearance(Appearance::Dark),
                "light" if args.is_empty() => Step::Appearance(Appearance::Light),
                "expect" if !rest.is_empty() => Step::Expect(rest.into()),
                "expect-focus" if args.is_empty() => Step::ExpectFocus,
                "quit" if args.is_empty() => Step::Quit,
                _ => return Err(error()),
            };
            steps.push_back(step);
        }
        if steps.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the script has no steps",
            ));
        }
        Ok(Self(steps))
    }
}

pub(crate) fn run(view: &mut AppView, mut script: Script) -> io::Result<()> {
    let started = Instant::now();
    // Waiting for an actual batch avoids machine-speed-dependent empty
    // first snapshots, while retaining a clear error for a dead dev server.
    while !view.has_frame() {
        advance(view, started)?;
        if started.elapsed() > Duration::from_secs(15) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no app frame arrived within 15 seconds",
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    while let Some(step) = script.0.pop_front() {
        let mut wait = Duration::from_millis(24);
        match step {
            Step::Wait(duration) => wait = duration,
            Step::Shot(path) => {
                std::fs::write(path, view.snapshot())?;
                wait = Duration::ZERO;
            }
            Step::Tap(column, row) => {
                mouse(view, MouseEventKind::Down(MouseButton::Left), column, row);
                mouse(view, MouseEventKind::Up(MouseButton::Left), column, row);
            }
            Step::Hover(column, row) => mouse(view, MouseEventKind::Moved, column, row),
            Step::Wheel(column, row, dx, dy) => {
                // Bound a single synthetic event burst. Large scrolls are
                // expressed as multiple script steps instead of starving IO.
                if dx.unsigned_abs() > 10_000 || dy.unsigned_abs() > 10_000 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "wheel step exceeds 10000 detents",
                    ));
                }
                for _ in 0..dy.unsigned_abs() {
                    mouse(
                        view,
                        if dy >= 0 {
                            MouseEventKind::ScrollDown
                        } else {
                            MouseEventKind::ScrollUp
                        },
                        column,
                        row,
                    );
                }
                for _ in 0..dx.unsigned_abs() {
                    mouse(
                        view,
                        if dx >= 0 {
                            MouseEventKind::ScrollRight
                        } else {
                            MouseEventKind::ScrollLeft
                        },
                        column,
                        row,
                    );
                }
            }
            Step::Key(key) => {
                view.handle_event(Event::Key(key));
            }
            Step::Type(text) => {
                view.handle_event(Event::Paste(text));
            }
            Step::Resize(columns, rows) => view.resize(columns, rows),
            Step::Appearance(appearance) => view.set_appearance(appearance),
            Step::Expect(text) => {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    advance(view, started)?;
                    let snapshot = view.snapshot();
                    if snapshot.contains(&text) {
                        break;
                    }
                    if Instant::now() >= deadline {
                        return Err(io::Error::other(format!(
                            "snapshot does not contain {text:?}:\n{snapshot}"
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                wait = Duration::ZERO;
            }
            Step::ExpectFocus => {
                if view.surface().focused().is_none() {
                    return Err(io::Error::other("the surface has no focused element"));
                }
                wait = Duration::ZERO;
            }
            Step::Quit => break,
        }
        let until = Instant::now().checked_add(wait).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "script wait duration exceeds the monotonic clock's range",
            )
        })?;
        loop {
            advance(view, started)?;
            if view.surface().should_quit() || Instant::now() >= until {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    view.stop();
    Ok(())
}

fn advance(view: &mut AppView, started: Instant) -> io::Result<()> {
    view.tick(started.elapsed().as_secs_f64())?;
    if let Some(error) = view.last_error() {
        return Err(io::Error::other(error.to_owned()));
    }
    Ok(())
}

fn mouse(view: &mut AppView, kind: MouseEventKind, column: u16, row: u16) {
    view.handle_event(Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    }));
}

fn key(name: &str) -> Option<KeyEvent> {
    let mut modifiers = KeyModifiers::empty();
    let mut parts = name.rsplitn(2, '+');
    let name = parts.next()?;
    if let Some(prefix) = parts.next() {
        for modifier in prefix.split('+') {
            modifiers |= match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" | "option" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                "super" | "cmd" | "command" => KeyModifiers::SUPER,
                _ => return None,
            };
        }
    }
    let code = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "esc" | "escape" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        value
            if value.starts_with('f')
                && value[1..]
                    .parse::<u8>()
                    .ok()
                    .is_some_and(|n| (1..=24).contains(&n)) =>
        {
            KeyCode::F(value[1..].parse().ok()?)
        }
        _ => {
            let mut chars = name.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(ch)
        }
    };
    Some(KeyEvent::new(code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_validates_before_taking_terminal_io() {
        assert!(Script::parse("resize 80 24; key Ctrl+Shift+Left; type 你好 👋; expect 你好; shot /tmp/screen with spaces.txt; quit").is_ok());
        assert!(Script::parse("resize -1 20").is_err());
        assert!(Script::parse("resize 0 20").is_err());
        assert!(Script::parse("key Typo").is_err());
        assert!(Script::parse("tpa 1 2").is_err());
    }

    #[test]
    fn modifiers_and_named_keys_are_real_terminal_events() {
        assert_eq!(
            key("Ctrl+Shift+Left"),
            Some(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ))
        );
        assert_eq!(
            key("Space"),
            Some(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()))
        );
        assert_eq!(
            key("F24"),
            Some(KeyEvent::new(KeyCode::F(24), KeyModifiers::empty()))
        );
        assert!(key("F25").is_none());
        assert!(key("hyper+x").is_none());
    }
}
