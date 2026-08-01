//! PTY driver: spawn the real `axon-tui` binary under a pseudo-terminal and
//! observe it through a modeled terminal screen.
//!
//! Output bytes feed both a `vt100` screen model (so assertions read rendered
//! rows, not raw ANSI) and a raw transcript retained for diagnostics. The raw
//! transcript also lets us assert on terminal control sequences the screen model
//! hides — notably the alternate-screen enter/leave pair, so a clean process
//! exit cannot mask a terminal-restoration regression.
//!
//! An optional `tee` receives every byte **verbatim** on its way to the screen
//! model. That is what makes `axon-demo-tui` possible (ADR 0086): graphics
//! escape sequences the `vt100` model discards — Sixel, Kitty, iTerm2 — are
//! just bytes, and survive being copied to a real terminal.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use portable_pty::{native_pty_system, Child, CommandBuilder, ExitStatus, MasterPty, PtySize};

// Crossterm uses 1049; some backends use 1047. Accept both so the check
// survives a crossterm version bump or backend swap.
const ALT_SCREEN_ENTER_SEQS: &[&[u8]] = &[b"\x1b[?1049h", b"\x1b[?1047h"];
const ALT_SCREEN_LEAVE_SEQS: &[&[u8]] = &[b"\x1b[?1049l", b"\x1b[?1047l"];

/// How long to nap between screen polls while waiting for a predicate.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A cloneable handle onto the child's keyboard.
///
/// Exists so a second thread can type: `axon-demo-tui` forwards the developer's
/// own stdin to the child while the script runs, which is what keeps Ctrl-C
/// working (the pilot holds the real terminal in raw mode, so Ctrl-C is a byte,
/// not a signal — the child has to receive it and quit).
#[derive(Clone)]
pub struct PtyInput {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl PtyInput {
    pub fn send_bytes(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().expect("writer lock");
        writer.write_all(bytes).context("write to pty")?;
        writer.flush().context("flush pty")?;
        Ok(())
    }
}

pub struct PtyDriver {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn Child + Send + Sync>,
    // Held so the PTY stays open for the lifetime of the driver; the writer and
    // reader were derived from it. Never read directly after construction.
    _master: Box<dyn MasterPty + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    raw: Arc<Mutex<Vec<u8>>>,
    // Latched by the reader thread the moment each sequence is detected.
    // O(1) reads; avoids cloning the full transcript on every poll.
    alt_entered: Arc<AtomicBool>,
    alt_left: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

/// Everything needed to spawn a child under a PTY.
///
/// A struct rather than more positional arguments because `tee` is the odd one
/// out: only the demo pilot sets it, and a bare `None` at a call site would say
/// nothing about what it is.
pub struct PtySpawn<'a> {
    pub bin: &'a Path,
    pub args: &'a [String],
    pub envs: &'a [(String, String)],
    pub cwd: &'a Path,
    pub rows: u16,
    pub cols: u16,
    /// Pixel size of the whole grid, when the caller knows it.
    ///
    /// Zero means "unknown", which is what a terminal that declines to report
    /// pixels also reports — the smoke harness always passes zero, since its
    /// child is not drawing to any real screen. The demo pilot passes the real
    /// numbers so anything in the child reading `TIOCGWINSZ` sees the truth
    /// rather than a PTY that claims to have no pixels.
    pub pixels: Option<(u16, u16)>,
    /// Verbatim sink for every byte the child writes, flushed per chunk.
    ///
    /// Written from the reader thread before the bytes reach the screen model,
    /// and never filtered: a graphics protocol the `vt100` parser drops still
    /// reaches this sink intact. A write error here is reported once on stderr
    /// and then ignored — a broken pipe must not take the child down.
    pub tee: Option<Box<dyn Write + Send>>,
}

impl<'a> PtySpawn<'a> {
    /// A spawn with no tee: the smoke harness's usual case.
    pub fn new(
        bin: &'a Path,
        args: &'a [String],
        envs: &'a [(String, String)],
        cwd: &'a Path,
        rows: u16,
        cols: u16,
    ) -> Self {
        Self {
            bin,
            args,
            envs,
            cwd,
            rows,
            cols,
            pixels: None,
            tee: None,
        }
    }
}

impl PtyDriver {
    /// Spawn `bin` with `args`, `envs`, and working directory `cwd` under a
    /// fixed-size PTY.
    pub fn spawn(
        bin: &Path,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<Self> {
        Self::spawn_with(PtySpawn::new(bin, args, envs, cwd, rows, cols))
    }

    /// Spawn with full control, including an optional verbatim output tee.
    pub fn spawn_with(spec: PtySpawn<'_>) -> anyhow::Result<Self> {
        let PtySpawn {
            bin,
            args,
            envs,
            cwd,
            rows,
            cols,
            pixels,
            mut tee,
        } = spec;
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: pixels.map_or(0, |(w, _)| w),
                pixel_height: pixels.map_or(0, |(_, h)| h),
            })
            .context("open pty")?;

        let mut cmd = CommandBuilder::new(bin);
        for arg in args {
            cmd.arg(arg);
        }
        for (key, value) in envs {
            cmd.env(key, value);
        }
        cmd.cwd(cwd);

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn {}", bin.display()))?;
        // The slave fd must be dropped in the parent so the child owns the only
        // copy; otherwise reads never see EOF when the child exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let writer = Arc::new(Mutex::new(
            pair.master.take_writer().context("take pty writer")?,
        ));

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let alt_entered = Arc::new(AtomicBool::new(false));
        let alt_left = Arc::new(AtomicBool::new(false));

        let reader_parser = Arc::clone(&parser);
        let reader_raw = Arc::clone(&raw);
        let reader_alt_entered = Arc::clone(&alt_entered);
        let reader_alt_left = Arc::clone(&alt_left);

        // Longest sequence in ALT_SCREEN_{ENTER,LEAVE}_SEQS is 8 bytes;
        // keep a 7-byte tail so sequences split across two reads are not missed.
        let overlap = ALT_SCREEN_ENTER_SEQS
            .iter()
            .chain(ALT_SCREEN_LEAVE_SEQS.iter())
            .map(|s| s.len())
            .max()
            .unwrap_or(0)
            .saturating_sub(1);

        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut prev_tail: Vec<u8> = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    // EINTR is recoverable; retry immediately.
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        eprintln!("smoke(pty): reader error: {e}");
                        break;
                    }
                    Ok(n) => {
                        let chunk = &buf[..n];
                        // Verbatim, and first: a recording should show the frame
                        // as soon as the child produced it, not after the screen
                        // model has finished parsing it.
                        if let Some(sink) = tee.as_mut() {
                            if let Err(e) = sink.write_all(chunk).and_then(|()| sink.flush()) {
                                eprintln!("smoke(pty): tee write failed, dropping tee: {e}");
                                tee = None;
                            }
                        }
                        reader_parser.lock().expect("parser lock").process(chunk);
                        reader_raw
                            .lock()
                            .expect("raw lock")
                            .extend_from_slice(chunk);

                        // Detect alt-screen sequences, handling splits across chunk
                        // boundaries by prepending the tail of the previous chunk.
                        let scan: Vec<u8> = prev_tail.iter().chain(chunk.iter()).copied().collect();
                        if !reader_alt_entered.load(Ordering::Relaxed)
                            && any_subslice(&scan, ALT_SCREEN_ENTER_SEQS)
                        {
                            reader_alt_entered.store(true, Ordering::Relaxed);
                        }
                        if !reader_alt_left.load(Ordering::Relaxed)
                            && any_subslice(&scan, ALT_SCREEN_LEAVE_SEQS)
                        {
                            reader_alt_left.store(true, Ordering::Relaxed);
                        }

                        let start = chunk.len().saturating_sub(overlap);
                        prev_tail.clear();
                        prev_tail.extend_from_slice(&chunk[start..]);
                    }
                }
            }
        });

        Ok(Self {
            writer,
            child,
            _master: pair.master,
            parser,
            raw,
            alt_entered,
            alt_left,
            reader: Some(handle),
        })
    }

    /// Write raw bytes to the terminal (keystrokes as the program would see them).
    pub fn send_bytes(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.input().send_bytes(bytes)
    }

    /// A handle another thread can type through. See [`PtyInput`].
    pub fn input(&self) -> PtyInput {
        PtyInput {
            writer: Arc::clone(&self.writer),
        }
    }

    /// Type a line of text (no trailing newline).
    pub fn type_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.send_bytes(text.as_bytes())
    }

    /// Press Enter (carriage return, as a terminal delivers it).
    pub fn press_enter(&mut self) -> anyhow::Result<()> {
        self.send_bytes(b"\r")
    }

    /// Send Ctrl-C (the `0x03` control byte).
    pub fn press_ctrl_c(&mut self) -> anyhow::Result<()> {
        self.send_bytes(&[0x03])
    }

    /// Current rendered screen as plain text (one line per row).
    pub fn screen_text(&self) -> String {
        self.parser.lock().expect("parser lock").screen().contents()
    }

    /// Raw output transcript captured so far.
    pub fn transcript(&self) -> Vec<u8> {
        self.raw.lock().expect("raw lock").clone()
    }

    /// Whether the program has entered the alternate screen.
    pub fn saw_alt_screen_enter(&self) -> bool {
        self.alt_entered.load(Ordering::Relaxed)
    }

    /// Whether the program has left the alternate screen (terminal restored).
    pub fn saw_alt_screen_leave(&self) -> bool {
        self.alt_left.load(Ordering::Relaxed)
    }

    /// Poll the rendered screen until `predicate` holds or `timeout` elapses.
    pub fn wait_for_screen<F>(
        &self,
        what: &str,
        timeout: Duration,
        predicate: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(&str) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            if predicate(&self.screen_text()) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "timed out after {:?} waiting for {what}\n--- screen ---\n{}",
                    timeout,
                    self.screen_text()
                ));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Poll the screen until `predicate` holds, failing fast if the process
    /// exits first. Used for first-paint gates so a binary that panics or exits
    /// before drawing fails immediately rather than after the full timeout.
    pub fn wait_for_screen_or_exit<F>(
        &mut self,
        what: &str,
        timeout: Duration,
        predicate: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(&str) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            if predicate(&self.screen_text()) {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait().context("poll child")? {
                return Err(anyhow!(
                    "process exited ({status:?}) before {what}\n--- screen ---\n{}",
                    self.screen_text()
                ));
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "timed out after {:?} waiting for {what}\n--- screen ---\n{}",
                    timeout,
                    self.screen_text()
                ));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Wait for the process to exit, returning its status.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> anyhow::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().context("poll child")? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(anyhow!("process did not exit within {timeout:?}"));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Returns the character contents of every cell in `col` across all rows,
    /// as `(row, contents)` pairs. Used to assert that a border column holds
    /// only box-drawing chars and not overflowed text.
    pub fn col_chars(&self, col: u16) -> Vec<(u16, String)> {
        let parser = self.parser.lock().expect("parser lock");
        let screen = parser.screen();
        (0..screen.size().0)
            .filter_map(|row| {
                screen
                    .cell(row, col)
                    .map(|c| (row, c.contents().to_owned()))
            })
            .collect()
    }

    /// Force-kill the process. Cleanup only — never a substitute for a clean exit.
    pub fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PtyDriver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Dropping the writer/master closes the fds, so the reader thread sees
        // EOF and exits; join it so no thread outlives the driver.
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

fn any_subslice(haystack: &[u8], needles: &[&[u8]]) -> bool {
    needles.iter().any(|n| contains_subslice(haystack, n))
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
