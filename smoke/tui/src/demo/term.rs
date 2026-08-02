//! The developer's real terminal: its geometry, its raw mode, and the guard
//! that puts it back.
//!
//! Everything here is about the *outer* terminal — the one a screen recorder is
//! pointed at. The child `axon-tui` gets its own PTY, sized to match.

use std::io::Write;

use anyhow::Context;

/// Terminal cell grid, and the pixel size of one cell when the terminal will
/// say (it usually will; `window_size` reads the TIOCGWINSZ ioctl rather than
/// asking the terminal over the wire, so it costs no escape-sequence round trip
/// and cannot leave an unanswered query in the input stream).
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub rows: u16,
    pub cols: u16,
    pub cell: Option<(u16, u16)>,
    pub cell_source: CellSource,
}

/// Where a cell size came from, for the run log — a wrong one is not an error,
/// it is a subtly wrong-sized image, so it has to be visible after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellSource {
    Xtwinops,
    Ioctl,
    Fallback,
}

impl CellSource {
    pub fn label(self) -> &'static str {
        match self {
            CellSource::Xtwinops => "terminal reply",
            CellSource::Ioctl => "ioctl",
            CellSource::Fallback => "fallback",
        }
    }
}

impl Geometry {
    pub fn detect() -> anyhow::Result<Self> {
        let (cols, rows) = crossterm::terminal::size().context("read terminal size")?;
        // Ask the terminal first, ioctl second — the same order `axon-tui` uses
        // when it is run by hand, and for the same reason: plenty of terminals
        // answer XTWINOPS accurately while leaving TIOCGWINSZ's pixel fields
        // zero. Falling back to a guessed cell size is what makes a previewed
        // image render smaller than the frame drawn around it, because the
        // sixel is encoded at `cells × cell size` and the terminal draws those
        // pixels at their true size.
        let cell = query_cell_size_via_xtwinops()
            .map(|cell| (cell, CellSource::Xtwinops))
            .or_else(|| {
                crossterm::terminal::window_size()
                    .ok()
                    .and_then(|w| cell_size(w.columns, w.rows, w.width, w.height))
                    .map(|cell| (cell, CellSource::Ioctl))
            });
        Ok(Self {
            rows,
            cols,
            cell: cell.map(|(cell, _)| cell),
            cell_source: cell.map_or(CellSource::Fallback, |(_, source)| source),
        })
    }
}

/// Ask the terminal for its cell size in pixels with XTWINOPS `CSI 16 t`, and
/// parse the `CSI 6 ; height ; width t` reply.
///
/// This is the query the *child* cannot run: its PTY has nobody on the far end
/// to answer. The pilot is attached to the developer's real terminal, so it can
/// ask on the child's behalf and hand the answer over as `AXON_FONT_SIZE`.
///
/// Inside tmux the query is wrapped in passthrough so it reaches the outer
/// terminal; tmux's own answer would describe the wrong thing. Reads go through
/// `poll(2)` with a hard deadline, so a terminal that never replies costs a
/// fraction of a second and no stuck thread.
#[cfg(unix)]
pub fn query_cell_size_via_xtwinops() -> Option<(u16, u16)> {
    use std::time::{Duration, Instant};

    // Only meaningful with a real terminal on both ends.
    if unsafe { libc::isatty(libc::STDIN_FILENO) != 1 || libc::isatty(libc::STDOUT_FILENO) != 1 } {
        return None;
    }
    let query: &[u8] = if std::env::var_os("TMUX").is_some() {
        b"\x1bPtmux;\x1b\x1b[16t\x1b\\"
    } else {
        b"\x1b[16t"
    };
    // Raw mode is set on STDIN directly rather than through crossterm, which
    // drives its own `/dev/tty` handle: the reply has no newline, so with
    // ICANON still on this descriptor `poll` never reports it readable and the
    // probe times out while the terminal echoes the answer into the output
    // instead. Restored below however this returns.
    //
    // ISIG goes too, for the restore's sake rather than the read's: this runs
    // before `RawGuard` exists and the `tcsetattr` below is a plain statement,
    // not a `Drop` guard, so a Ctrl-C inside the ~300ms window would raise
    // SIGINT, kill the process, and leave the terminal half-raw. Cleared, that
    // Ctrl-C arrives as a `0x03` byte instead — which `drain_pending_input`
    // discards on the way to the first forwarded keystroke.
    let saved = unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut termios) != 0 {
            return None;
        }
        let mut raw = termios;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
            return None;
        }
        termios
    };
    let response = (|| {
        let mut out = std::io::stdout();
        out.write_all(query).ok()?;
        out.flush().ok()?;

        let deadline = Instant::now() + Duration::from_millis(300);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 64];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let mut fds = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut fds, 1, remaining.as_millis() as libc::c_int) };
            if ready <= 0 {
                return None;
            }
            // Read the descriptor directly rather than through `std::io::stdin`,
            // which is buffered: it would drain the whole reply into its own
            // userspace buffer, hand back the byte asked for, and leave `poll`
            // reporting nothing readable for the rest — the answer sitting in a
            // buffer while the probe times out waiting for it.
            let n = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    chunk.as_mut_ptr() as *mut libc::c_void,
                    chunk.len(),
                )
            };
            if n <= 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n as usize]);
            if buf.contains(&b't') {
                return Some(buf);
            }
            if buf.len() > 64 {
                return None;
            }
        }
    })();
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved);
    }
    parse_cell_size_response(&response?)
}

#[cfg(not(unix))]
pub fn query_cell_size_via_xtwinops() -> Option<(u16, u16)> {
    None
}

/// Discard anything still sitting in the terminal's input queue.
///
/// Run between the cell-size probe and the first forwarded keystroke. A
/// terminal may answer a query in more than one chunk, or volunteer a reply we
/// never asked for; whatever is left behind would otherwise be forwarded into
/// the child as typed input. That is not theoretical — a leftover `[6;21;10t`
/// once prefixed a scripted command, which stopped it parsing as a command and
/// sent it into the room as a chat message.
#[cfg(unix)]
pub fn drain_pending_input() {
    let mut chunk = [0u8; 256];
    loop {
        let mut fds = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // Zero timeout: this only ever discards what has already arrived.
        if unsafe { libc::poll(&mut fds, 1, 0) } <= 0 {
            return;
        }
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                chunk.as_mut_ptr() as *mut libc::c_void,
                chunk.len(),
            )
        };
        if n <= 0 {
            return;
        }
    }
}

#[cfg(not(unix))]
pub fn drain_pending_input() {}

/// `CSI 6 ; height ; width t` → `(width, height)` in pixels.
fn parse_cell_size_response(response: &[u8]) -> Option<(u16, u16)> {
    let text = String::from_utf8_lossy(response);
    let start = text.find("[6;")?;
    let values = text[start + 3..].split_once('t')?.0;
    let (height, width) = values.split_once(';')?;
    let height: u16 = height.trim().parse().ok()?;
    let width: u16 = width.trim().parse().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

/// Per-cell pixels, from the window's pixel dimensions over its cell grid.
///
/// Mirrors `font_size_from_window` in `clients/tui/src/main.rs` — the same
/// arithmetic the TUI would do for itself if we let it query. Zero anywhere
/// means the terminal declined to report pixels (many do), not a small cell.
fn cell_size(columns: u16, rows: u16, width: u16, height: u16) -> Option<(u16, u16)> {
    if columns == 0 || rows == 0 || width == 0 || height == 0 {
        return None;
    }
    let cell_width = width / columns;
    let cell_height = height / rows;
    (cell_width > 0 && cell_height > 0).then_some((cell_width, cell_height))
}

/// Puts the outer terminal in raw mode and guarantees it comes back out.
///
/// Raw mode matters for two reasons. Keystrokes the developer accidentally
/// types are not echoed into the middle of a recording, and — because the pilot
/// forwards its own stdin to the child — the child sees keys byte for byte,
/// including the Ctrl-C its quit shortcut is bound to. The trade is that Ctrl-C
/// no longer signals the *pilot*, which is why the child must be the thing that
/// quits.
pub struct RawGuard {
    #[cfg(unix)]
    saved: Option<libc::termios>,
}

#[cfg(unix)]
impl RawGuard {
    pub fn enter() -> anyhow::Result<Self> {
        // Set directly on STDIN rather than through crossterm, which drives its
        // own `/dev/tty` handle: this guard has to govern exactly the descriptor
        // the stdin forwarder reads, so that keystrokes reach the child byte by
        // byte instead of a line at a time.
        let saved = unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut termios) != 0 {
                anyhow::bail!("read terminal attributes");
            }
            let mut raw = termios;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                anyhow::bail!("enable raw mode");
            }
            termios
        };
        Ok(Self { saved: Some(saved) })
    }

    /// Restore the terminal early, so anything printed afterwards lands in a
    /// cooked terminal. Idempotent; `Drop` will not double-restore.
    pub fn restore(&mut self) {
        if let Some(saved) = self.saved.take() {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved);
            }
        }
    }
}

#[cfg(not(unix))]
impl RawGuard {
    pub fn enter() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode().context("enable raw mode")?;
        Ok(Self {})
    }

    pub fn restore(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Best-effort repair of a terminal a killed child left in the alternate
/// screen: leave it, show the cursor, reset attributes.
///
/// Only reached when the child did *not* exit cleanly — a clean `/quit` emits
/// all of this itself, and the tee has already carried it through.
pub fn repair_terminal() {
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[?1049l\x1b[?25h\x1b[0m");
    let _ = out.flush();
}
