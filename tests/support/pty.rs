//! Drives `tenuto` inside a pseudo-terminal, for tests that need a real
//! terminal on the child's stdin, stdout and stderr (M5 §12). The child is
//! launched from [`crate::process::binary`] with [`crate::process::profile_env`]
//! applied, so it gets the same isolated profile as every other launch.
//!
//! Every wait has a caller-supplied patience, and dropping a `PtyChild` whose
//! child is still running kills it, so a failing assertion never leaves a
//! process behind or hangs the suite.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::process;

/// How often a wait rechecks the output or the child.
const POLL: Duration = Duration::from_millis(20);
/// How long `wait_exit` lets the reader collect what the child wrote before
/// it exited. The reader sees end of file once the last slave descriptor is
/// closed, which normally happens with the child's exit.
const DRAIN_PATIENCE: Duration = Duration::from_secs(2);
/// How long `Drop` waits for a killed child to be reaped.
const KILL_PATIENCE: Duration = Duration::from_secs(2);
/// Variables a test must opt into explicitly: an inherited hook or output
/// selection would silently change what the child does.
const OPT_IN_VARIABLES: [&str; 2] = ["TENUTO_TEST_HOOK", "TENUTO_AUDIO_OUTPUT"];
/// The terminal every child is told it runs in, whatever terminal runs the
/// suite. Under tmux, image protocol detection runs
/// `tmux set -p allow-passthrough on`, which would change the developer's own
/// pane; without `TMUX` and `TERM_PROGRAM` the child cannot tell it is there.
const CHILD_TERM: &str = "xterm-256color";
const HOST_TERMINAL_VARIABLES: [&str; 3] = ["TMUX", "TMUX_PANE", "TERM_PROGRAM"];

pub struct PtyChild {
    child: Box<dyn Child + Send + Sync>,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    output: Arc<Mutex<Vec<u8>>>,
    reader_done: Arc<AtomicBool>,
    /// Asks the reader thread to drop its own master descriptor after its
    /// next read, which is what completes a hangup (see `close_master`).
    hang_up: Arc<AtomicBool>,
    exit_code: Option<Option<u32>>,
    cols: u16,
    rows: u16,
}

fn lock(output: &Mutex<Vec<u8>>) -> MutexGuard<'_, Vec<u8>> {
    match output.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

/// The command `PtyChild::spawn` runs: `tenuto <args>` in `root` with its
/// isolated profile, the opt-in variables removed, a fixed terminal type and
/// no trace of a multiplexer, then `env` on top.
pub fn command(root: &Path, args: &[&str], env: &[(&str, &str)]) -> CommandBuilder {
    let mut command = CommandBuilder::new(process::binary());
    command.args(args);
    command.cwd(root);
    for (key, value) in process::profile_env(root) {
        command.env(key, value);
    }
    for key in OPT_IN_VARIABLES.into_iter().chain(HOST_TERMINAL_VARIABLES) {
        command.env_remove(key);
    }
    command.env("TERM", CHILD_TERM);
    command.env("TENUTO_MPRIS", "off");
    for (key, value) in env {
        command.env(key, value);
    }
    command
}

impl PtyChild {
    /// Launches `tenuto <args>` on a new `cols`×`rows` PTY under the
    /// profile at `root`. `TENUTO_TEST_HOOK` and `TENUTO_AUDIO_OUTPUT`
    /// are removed unless `env` sets them.
    pub fn spawn(
        root: &Path,
        args: &[&str],
        env: &[(&str, &str)],
        cols: u16,
        rows: u16,
    ) -> std::io::Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_error)?;

        let child = pair
            .slave
            .spawn_command(command(root, args, env))
            .map_err(io_error)?;
        // The child holds its own copies; keeping ours open would stop the
        // reader from ever seeing end of file.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().map_err(io_error)?;
        let writer = pair.master.take_writer().map_err(io_error)?;
        let output = Arc::new(Mutex::new(Vec::new()));
        let reader_done = Arc::new(AtomicBool::new(false));
        let hang_up = Arc::new(AtomicBool::new(false));
        {
            let output = Arc::clone(&output);
            let reader_done = Arc::clone(&reader_done);
            let hang_up = Arc::clone(&hang_up);
            std::thread::Builder::new()
                .name("pty-reader".to_string())
                .spawn(move || {
                    let mut buffer = [0_u8; 4096];
                    loop {
                        match reader.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => lock(&output).extend_from_slice(&buffer[..read]),
                        }
                        if hang_up.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    drop(reader);
                    reader_done.store(true, Ordering::SeqCst);
                })?;
        }

        Ok(Self {
            child,
            master: Some(pair.master),
            writer: Some(writer),
            output,
            reader_done,
            hang_up,
            exit_code: None,
            cols,
            rows,
        })
    }

    /// Types `bytes` into the terminal. A write to a closed PTY is ignored:
    /// the assertion that follows reports the real problem better.
    pub fn send(&mut self, bytes: &[u8]) {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    /// Whether `needle` appears within `patience`, either verbatim in the raw
    /// output or on one row of the screen that output draws. The screen form
    /// matters because Ratatui skips unchanged blank cells and moves the
    /// cursor over them, so a phrase with spaces is rarely contiguous in the
    /// raw bytes.
    pub fn wait_for(&self, needle: &str, patience: Duration) -> bool {
        let deadline = Instant::now() + patience;
        loop {
            let bytes = lock(&self.output).clone();
            if String::from_utf8_lossy(&bytes).contains(needle)
                || Screen::render(&bytes, self.cols, self.rows)
                    .rows()
                    .any(|row| row.contains(needle))
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }

    /// Everything the child has written so far, escape sequences included.
    pub fn output(&self) -> String {
        String::from_utf8_lossy(&lock(&self.output)).into_owned()
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// The child's exit code once it exits within `patience`, after giving
    /// the reader a bounded moment to collect the last output. `None` when
    /// the child is still running at the deadline (it is killed) or was ended
    /// by a signal rather than exiting.
    pub fn wait_exit(&mut self, patience: Duration) -> Option<u32> {
        if let Some(code) = self.exit_code {
            return code;
        }
        let deadline = Instant::now() + patience;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
                Ok(None) | Err(_) => break None,
            }
        };
        let Some(status) = status else {
            self.kill();
            return None;
        };
        let drain_deadline = Instant::now() + DRAIN_PATIENCE;
        while !self.reader_done.load(Ordering::SeqCst) && Instant::now() < drain_deadline {
            std::thread::sleep(POLL);
        }
        let code = status.signal().is_none().then(|| status.exit_code());
        self.exit_code = Some(code);
        code
    }

    /// Hangs up the terminal: drops the master and its writer, and asks the
    /// reader to drop its descriptor too. The kernel hangs up only once every
    /// master descriptor is closed, so the hangup completes when the reader
    /// next returns — at the child's next write, or at its exit.
    pub fn close_master(&mut self) {
        self.hang_up.store(true, Ordering::SeqCst);
        self.writer = None;
        self.master = None;
    }

    /// Hangs up the way a closed pane does, with no input on the way out.
    /// Dropping portable-pty's writer types a newline and the terminal's EOF
    /// character, which the child would read as two keys before the hangup;
    /// undefining EOF on the terminal first leaves the writer nothing to type.
    pub fn close_master_silently(&mut self) {
        let tty = self.master.as_ref().and_then(|master| master.tty_name());
        let silenced = tty.is_some_and(|tty| {
            std::process::Command::new("stty")
                .arg("-F")
                .arg(tty)
                .args(["eof", "undef"])
                .status()
                .is_ok_and(|status| status.success())
        });
        assert!(silenced, "could not undefine EOF on the child's terminal");
        self.close_master();
    }

    /// portable-pty's kill sends SIGHUP, waits about 200 ms for the child to
    /// exit, then sends SIGKILL, so a child that catches SIGHUP (every
    /// `tenuto` player does) or is stuck is still ended.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let deadline = Instant::now() + KILL_PATIENCE;
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                break;
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        if self.exit_code.is_none() && matches!(self.child.try_wait(), Ok(None)) {
            self.kill();
        }
    }
}

/// The text a terminal would show after `bytes`, for matching phrases. Only
/// what Ratatui and crossterm emit is interpreted: printable characters,
/// carriage return and line feed, absolute and forward cursor moves, and
/// erasing. Any other escape sequence is skipped. Switching screens clears
/// the grid.
struct Screen {
    grid: Vec<Vec<char>>,
    row: usize,
    col: usize,
}

impl Screen {
    fn render(bytes: &[u8], cols: u16, rows: u16) -> Self {
        let (cols, rows) = (usize::from(cols.max(1)), usize::from(rows.max(1)));
        let mut screen = Self {
            grid: vec![vec![' '; cols]; rows],
            row: 0,
            col: 0,
        };
        let text = String::from_utf8_lossy(bytes);
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\x1b' => match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        let mut last = None;
                        for next in chars.by_ref() {
                            if ('\x40'..='\x7e').contains(&next) {
                                last = Some(next);
                                break;
                            }
                            params.push(next);
                        }
                        if let Some(last) = last {
                            screen.csi(&params, last);
                        }
                    }
                    // OSC, DCS and APC strings run to BEL or ST.
                    Some(']' | 'P' | '_') => {
                        while let Some(next) = chars.next() {
                            if next == '\x07' {
                                break;
                            }
                            if next == '\x1b' && chars.peek() == Some(&'\\') {
                                chars.next();
                                break;
                            }
                        }
                    }
                    _ => {}
                },
                '\r' => screen.col = 0,
                '\n' => screen.line_feed(),
                '\x08' => screen.col = screen.col.saturating_sub(1),
                ch if ch.is_control() => {}
                ch => screen.put(ch),
            }
        }
        screen
    }

    fn rows(&self) -> impl Iterator<Item = String> + '_ {
        self.grid.iter().map(|row| row.iter().collect())
    }

    fn put(&mut self, ch: char) {
        let cols = self.grid.first().map_or(0, Vec::len);
        if self.col >= cols {
            self.col = 0;
            self.line_feed();
        }
        if let Some(cell) = self
            .grid
            .get_mut(self.row)
            .and_then(|row| row.get_mut(self.col))
        {
            *cell = ch;
        }
        self.col += 1;
    }

    fn line_feed(&mut self) {
        if self.row + 1 < self.grid.len() {
            self.row += 1;
        } else {
            let cols = self.grid.first().map_or(0, Vec::len);
            self.grid.remove(0);
            self.grid.push(vec![' '; cols]);
        }
    }

    fn clear(&mut self) {
        for row in &mut self.grid {
            row.fill(' ');
        }
    }

    fn csi(&mut self, params: &str, last: char) {
        let numbers: Vec<usize> = params
            .trim_start_matches('?')
            .split(';')
            .map(|part| part.parse().unwrap_or(0))
            .collect();
        let first = numbers.first().copied().unwrap_or(0);
        let rows = self.grid.len();
        let cols = self.grid.first().map_or(0, Vec::len);
        match last {
            'H' | 'f' => {
                let second = numbers.get(1).copied().unwrap_or(0);
                self.row = first.max(1).min(rows) - 1;
                self.col = second.max(1).min(cols) - 1;
            }
            'C' => self.col = (self.col + first.max(1)).min(cols.saturating_sub(1)),
            'J' if first == 2 || first == 3 => self.clear(),
            'J' => {
                let (row, col) = (self.row, self.col);
                for (index, line) in self.grid.iter_mut().enumerate().skip(row) {
                    let from = if index == row { col.min(line.len()) } else { 0 };
                    line[from..].fill(' ');
                }
            }
            'K' => {
                if let Some(line) = self.grid.get_mut(self.row) {
                    let from = self.col.min(line.len());
                    line[from..].fill(' ');
                }
            }
            'h' | 'l' if params.starts_with('?') && numbers.contains(&1049) => {
                self.clear();
                self.row = 0;
                self.col = 0;
            }
            _ => {}
        }
    }
}
