//! Terminal output and input. Progress goes to stderr so stdout stays clean for
//! `--json` results. Write errors are ignored: a closed popup must not panic us
//! halfway through creating a machine.

use std::fmt::Display;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

static JSON: AtomicBool = AtomicBool::new(false);

pub fn set_json(on: bool) {
    JSON.store(on, Ordering::Relaxed);
}

pub fn json() -> bool {
    JSON.load(Ordering::Relaxed)
}

pub fn interactive() -> bool {
    !json() && io::stdin().is_terminal()
}

fn color() -> bool {
    io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

pub fn paint(code: &str, text: impl Display) -> String {
    if color() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn bold(text: impl Display) -> String {
    paint("1", text)
}

pub fn dim(text: impl Display) -> String {
    paint("2", text)
}

fn err_line(line: &str) {
    let mut err = io::stderr();
    let _ = writeln!(err, "{line}");
    let _ = err.flush();
}

pub fn step(msg: impl Display) {
    err_line(&format!("{} {msg}", paint("36", "›")));
}

pub fn done(msg: impl Display) {
    err_line(&format!("{} {msg}", paint("32", "✓")));
}

pub fn warn(msg: impl Display) {
    err_line(&format!("{} {msg}", paint("33", "!")));
}

pub fn fail(msg: impl Display) {
    err_line(&format!("{} {msg}", paint("31", "✗")));
}

pub fn note(msg: impl Display) {
    err_line(&format!("  {}", dim(msg)));
}

/// Human output on stdout (tables, final hints).
pub fn out(line: impl Display) {
    let mut out = io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

pub fn print_json(value: &serde_json::Value) {
    out(serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()));
}

pub fn clear() {
    if io::stdout().is_terminal() {
        let mut out = io::stdout();
        let _ = write!(out, "\x1b[2J\x1b[H");
        let _ = out.flush();
    }
}

fn prompt(text: &str) {
    let mut err = io::stderr();
    let _ = write!(err, "{text}");
    let _ = err.flush();
}

pub fn read_line() -> Result<String> {
    let mut line = String::new();
    let n = io::stdin()
        .lock()
        .read_line(&mut line)
        .context("read input")?;
    if n == 0 {
        anyhow::bail!(crate::Coded::new("cancelled", "input closed"));
    }
    Ok(line.trim().to_owned())
}

pub fn ask(question: &str) -> Result<String> {
    prompt(&format!("{} ", bold(question)));
    read_line()
}

/// Reads a secret without echo when stdin is a terminal.
pub fn ask_secret(question: &str) -> Result<String> {
    if !io::stdin().is_terminal() {
        return read_line();
    }
    prompt(&format!("{} ", bold(question)));
    let _guard = RawGuard::new(|t| t.c_lflag &= !libc::ECHO)?;
    let line = read_line();
    prompt("\n");
    line
}

/// One key press. Esc and Ctrl-C come back as `Key::Cancel`; arrow keys and
/// other escape sequences are swallowed instead of leaking into the next read.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Key {
    Char(char),
    Enter,
    Cancel,
}

pub fn read_key() -> Result<Key> {
    if !io::stdin().is_terminal() {
        let line = read_line()?;
        return Ok(match line.chars().next() {
            None => Key::Enter,
            Some(ch) => Key::Char(ch),
        });
    }
    let _guard = RawGuard::new(|t| {
        t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
    })?;
    let mut byte = [0u8; 1];
    io::stdin().read_exact(&mut byte).context("read key")?;
    Ok(match byte[0] {
        b'\r' | b'\n' => Key::Enter,
        0x03 | 0x04 => Key::Cancel,
        0x1b => {
            drain_escape();
            Key::Cancel
        }
        b => Key::Char(b as char),
    })
}

fn drain_escape() {
    let fd = io::stdin().as_raw_fd();
    let mut pending = 0i32;
    // SAFETY: FIONREAD writes one int into `pending`.
    if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut pending) } == 0 && pending > 0 {
        let mut buf = vec![0u8; pending as usize];
        let _ = io::stdin().read(&mut buf);
    }
}

/// y/N question. Enter takes the default; Esc says no.
pub fn confirm(question: &str, default: bool) -> Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    prompt(&format!("{} {} ", bold(question), dim(hint)));
    let key = read_key()?;
    let yes = match key {
        Key::Char('y' | 'Y') => true,
        Key::Char('n' | 'N') | Key::Cancel => false,
        Key::Enter => default,
        Key::Char(_) => false,
    };
    prompt(if yes { "yes\n" } else { "no\n" });
    Ok(yes)
}

pub fn pause() {
    prompt(&dim("press any key…"));
    let _ = read_key();
    prompt("\n");
}

struct RawGuard {
    fd: i32,
    original: libc::termios,
}

impl RawGuard {
    fn new(edit: impl FnOnce(&mut libc::termios)) -> Result<Self> {
        let fd = io::stdin().as_raw_fd();
        // SAFETY: termios is plain data; tcgetattr fills it.
        let mut original = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error()).context("terminal attributes");
        }
        let mut raw = original;
        edit(&mut raw);
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error()).context("terminal mode");
        }
        Ok(Self { fd, original })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        // SAFETY: restores the attributes read in `new`.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}
