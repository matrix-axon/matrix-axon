mod api;
mod app;
mod args;
mod command;
mod config;
mod html;
mod keymap;
mod search;
mod ui;
mod wrap;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{websocket_task, AxonClient};
use app::{App, LiveFrameAction, Mode, PopupKind};
use args::{normalize_token, Args};
use config::TuiConfig;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEvent, KeyEventKind,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::FontSize;
use tokio::sync::mpsc;
use tokio::time;
use ui::draw;
use uuid::Uuid;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let config = TuiConfig::load_or_create_default()?;
    let base_url = args
        .base_url
        .or_else(|| config.base_url.clone())
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_owned());
    let token = args.token.or_else(|| normalize_token(config.token.clone()));
    if let Some(ref t) = token {
        if reqwest::header::HeaderValue::from_str(&api::bearer_value_str(t)).is_err() {
            eprintln!(
                "Error: the bearer token contains characters that are invalid in an HTTP header \
                 value (e.g. control characters or non-ASCII bytes).\n\
                 \nCheck your config file ({path}) or AXON_TOKEN environment variable.",
                path = config.path.display()
            );
            std::process::exit(1);
        }
    }
    let client = AxonClient::new(base_url, token.clone());

    if token.is_none() {
        if let Err(api::ApiError::Status { status, .. }) = client.list_accounts().await {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                eprintln!(
                    "Error: this Axon server requires authentication but no bearer token is configured.\n\
                     \nMint a token on the server:\n\
                     \n    axon token issue --label <name>\n\
                     \nThen add it to your config file ({path}):\n\
                     \n    [server]\n    bearer_token = \"<token>\"\n\
                     \nOr supply it at launch:\n\
                     \n    axon-tui --token <token>\n    AXON_TOKEN=<token> axon-tui",
                    path = config.path.display()
                );
                std::process::exit(1);
            }
        }
    }

    // Do not use Picker::from_query_stdio(): on terminals that don't answer its
    // capability query, ratatui-image leaves a detached stdin reader behind. That
    // reader can race the TUI input thread and silently consume keystrokes. Instead
    // terminal_image_picker() uses environment hints plus a bounded, poll-based DA1
    // probe (for Sixel) that runs here, before the input thread starts, and never
    // leaves a thread blocked on stdin.
    let picker = terminal_image_picker();
    let mut terminal = TerminalGuard::enter()?;
    let result = run_app(
        &mut terminal.terminal,
        client,
        args.account_id,
        config,
        picker,
    )
    .await;
    terminal.leave()?;
    result
}

fn terminal_image_picker() -> Picker {
    // Halfblocks works without a real font size, but the graphics protocols
    // (iTerm2/Kitty/Sixel) encode each image at `cells * font_size` pixels and
    // then ask the terminal to draw it at exactly that pixel size. Picker::halfblocks()
    // hardcodes an arbitrary 10x20 cell, so on terminals whose real cells differ
    // (notably Retina iTerm2) every graphic comes out scaled wrong. Seed the picker
    // with the terminal's actual cell size, derived from the TIOCGWINSZ ioctl via
    // crossterm::window_size(). Inside tmux those pixel dimensions describe the
    // outer window while rows/columns can describe only the pane, so we refuse
    // that mismatched ratio unless AXON_FONT_SIZE supplies an explicit value.
    if inside_tmux() {
        enable_tmux_passthrough();
    }
    let font_size = query_font_size();
    let mut picker = picker_with_tmux_state(font_size);
    // GNU screen has no reliable pixel-graphics passthrough (and historically a
    // small DCS string buffer), so Sixel/Kitty/iTerm2 sequences come out mangled
    // or empty.  Force halfblocks, which are ordinary cells every multiplexer
    // forwards safely, and skip the capability probes (their terminal I/O is
    // pointless here and could mislead).
    if inside_screen() {
        picker.set_protocol_type(ProtocolType::Halfblocks);
        return picker;
    }
    // Inside tmux, graphics protocols need an explicit pixel-to-cell ratio:
    // tmux's reported pixel dimensions can describe the outer window rather
    // than the current pane. Outside tmux, retain ratatui-image's established
    // fallback behavior; it works on terminals such as Windows Terminal that
    // advertise Sixel but do not report pixel dimensions through TIOCGWINSZ.
    if font_size.is_none() && inside_tmux() {
        return picker;
    }
    let protocol = std::env::var("AXON_IMAGE_PROTOCOL")
        .ok()
        .and_then(|value| parse_image_protocol(&value))
        .or_else(detect_image_protocol_from_env)
        // Sixel has no reliable environment variable, so it can only be found by
        // asking the terminal directly. Run this last: it does terminal I/O, and
        // the env checks already cover iTerm2/Kitty without touching the tty.
        .or_else(detect_sixel_via_da1);
    if let Some(protocol) = protocol {
        picker.set_protocol_type(protocol);
    }
    picker
}

fn picker_with_tmux_state(font_size: Option<FontSize>) -> Picker {
    // ratatui-image 11 recognizes tmux only from TERM=tmux* or
    // TERM_PROGRAM=tmux, while many real tmux sessions use TERM=screen-* and
    // expose only TMUX. Temporarily provide the hint its constructor expects so
    // generated Sixel/iTerm2/Kitty sequences use tmux passthrough. Restore the
    // user's TERM immediately; this runs before any worker threads are started.
    let needs_tmux_hint = inside_tmux()
        && !std::env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
        && !std::env::var("TERM_PROGRAM").is_ok_and(|program| program == "tmux");
    let original_term = std::env::var_os("TERM");
    if needs_tmux_hint {
        std::env::set_var("TERM", "tmux-256color");
    }

    let picker = match font_size {
        Some(font_size) => {
            // from_fontsize is the only public way to inject a font size in 11.x.
            #[allow(deprecated)]
            {
                Picker::from_fontsize(font_size)
            }
        }
        None => Picker::halfblocks(),
    };

    if needs_tmux_hint {
        if let Some(original_term) = original_term {
            std::env::set_var("TERM", original_term);
        } else {
            std::env::remove_var("TERM");
        }
    }
    picker
}

fn enable_tmux_passthrough() {
    let _ = std::process::Command::new("tmux")
        .args(["set", "-p", "allow-passthrough", "on"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Probe the terminal for Sixel support with a Primary Device Attributes (DA1)
/// query: write `ESC [ c` and look for attribute `4` in the `ESC [ ? … c` reply.
///
/// Inside tmux the query must reach the *outer* terminal, not tmux itself: tmux
/// answers DA1 on its own and, when built with Sixel support, advertises Sixel
/// (attribute 4) regardless of whether the outer terminal can display it — a
/// guaranteed false positive. So inside tmux we wrap the query in tmux passthrough
/// (`ESC P tmux; … ESC \`), which tmux forwards to the outer terminal so its real
/// DA1 comes back. If passthrough is disabled the query is dropped and nothing
/// returns, which correctly resolves to halfblocks.
///
/// Unlike `Picker::from_query_stdio()`, this never spawns a thread that can be
/// left blocked on stdin. It runs at startup, before the input thread exists, and
/// reads the tty through `poll(2)` with a hard deadline, so it can neither hang
/// nor race keystroke handling. Any reply bytes are fully drained here rather than
/// leaking into the event loop.
fn detect_sixel_via_da1() -> Option<ProtocolType> {
    if std::env::var_os("AXON_NO_IMAGE_QUERY").is_some() {
        return None;
    }
    // Only meaningful against a real terminal on both ends.
    if !is_real_tty() {
        return None;
    }
    // Inside tmux, ask the outer terminal via passthrough; otherwise query directly.
    let query: &[u8] = if inside_tmux() {
        b"\x1bPtmux;\x1b\x1b[c\x1b\\"
    } else {
        b"\x1b[c"
    };
    // The reply needs raw mode so it arrives byte-for-byte without echo or line
    // buffering. Restore the prior mode regardless of how we leave.
    if crossterm::terminal::enable_raw_mode().is_err() {
        return None;
    }
    let response = read_terminal_response(query, b'c');
    let _ = crossterm::terminal::disable_raw_mode();

    if sixel_in_da1(&response?) {
        Some(ProtocolType::Sixel)
    } else {
        None
    }
}

/// True if both stdin and stdout are connected to a real terminal. The DA1/XTWINOPS
/// probes below only make sense against a real tty on both ends, and rely on raw
/// POSIX fd APIs that don't exist on Windows, so there we just say no and every
/// caller falls back to Halfblocks / non-query behavior.
#[cfg(unix)]
fn is_real_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 }
}

#[cfg(not(unix))]
fn is_real_tty() -> bool {
    false
}

/// Send a DA1 query and collect the reply, polling the tty with a bounded deadline
/// so a non-responding terminal (or a dropped tmux passthrough) can't stall
/// startup. Returns the raw bytes read, or None if nothing arrived.
#[cfg(unix)]
fn read_terminal_response(query: &[u8], terminator: u8) -> Option<Vec<u8>> {
    use std::io::Write;

    let mut stdout = io::stdout();
    stdout.write_all(query).ok()?;
    stdout.flush().ok()?;

    let deadline = Instant::now() + Duration::from_millis(250);
    let mut buf = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll_fd points to a valid, initialized pollfd for the duration.
        let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if ready <= 0 || poll_fd.revents & libc::POLLIN == 0 {
            break; // timeout, error, or hangup
        }
        let mut chunk = [0u8; 64];
        // SAFETY: chunk is a valid, sized buffer; read writes at most chunk.len() bytes.
        let read = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                chunk.as_mut_ptr().cast::<libc::c_void>(),
                chunk.len(),
            )
        };
        if read <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read as usize]);
        if buf.contains(&terminator) || buf.len() > 256 {
            break;
        }
    }
    (!buf.is_empty()).then_some(buf)
}

#[cfg(not(unix))]
fn read_terminal_response(_query: &[u8], _terminator: u8) -> Option<Vec<u8>> {
    None
}

/// True if a DA1 reply (`ESC [ ? Ps ; Ps ; … c`) advertises Sixel, which is
/// attribute `4` among the semicolon-separated parameters.
fn sixel_in_da1(response: &[u8]) -> bool {
    let text = String::from_utf8_lossy(response);
    let Some(start) = text.find("[?") else {
        return false;
    };
    let after = &text[start + 2..];
    let Some(end) = after.find('c') else {
        return false;
    };
    after[..end].split(';').any(|param| param.trim() == "4")
}

/// Determine the terminal cell size in pixels, used to scale graphics-protocol
/// images correctly. An explicit `AXON_FONT_SIZE=WxH` override wins. Otherwise,
/// ask the terminal directly with XTWINOPS `CSI 16 t`, forwarding the query
/// through tmux passthrough when necessary. As a final non-tmux fallback, derive
/// the ratio from TIOCGWINSZ. We never use that ratio inside tmux because its
/// pixel dimensions may describe the outer window while rows/columns describe
/// only the current pane.
fn query_font_size() -> Option<FontSize> {
    if let Some(font_size) = std::env::var("AXON_FONT_SIZE")
        .ok()
        .and_then(|value| parse_font_size(&value))
    {
        return Some(font_size);
    }
    if std::env::var_os("AXON_NO_IMAGE_QUERY").is_some() {
        return None;
    }
    if let Some(font_size) = query_cell_size_via_xtwinops() {
        return Some(font_size);
    }
    if inside_tmux() {
        return None;
    }
    let window = crossterm::terminal::window_size().ok()?;
    font_size_from_window(window.columns, window.rows, window.width, window.height)
}

fn query_cell_size_via_xtwinops() -> Option<FontSize> {
    if !is_real_tty() {
        return None;
    }
    let query: &[u8] = if inside_tmux() {
        b"\x1bPtmux;\x1b\x1b[16t\x1b\\"
    } else {
        b"\x1b[16t"
    };
    if crossterm::terminal::enable_raw_mode().is_err() {
        return None;
    }
    let response = read_terminal_response(query, b't');
    let _ = crossterm::terminal::disable_raw_mode();
    parse_cell_size_response(&response?)
}

fn parse_cell_size_response(response: &[u8]) -> Option<FontSize> {
    let text = String::from_utf8_lossy(response);
    let start = text.find("[6;")?;
    let values = text[start + 3..].split_once('t')?.0;
    let (height, width) = values.split_once(';')?;
    let height: u16 = height.parse().ok()?;
    let width: u16 = width.parse().ok()?;
    (width > 0 && height > 0).then(|| FontSize::new(width, height))
}

/// Divide the window's pixel dimensions by its cell grid to get per-cell pixels.
fn font_size_from_window(columns: u16, rows: u16, width: u16, height: u16) -> Option<FontSize> {
    if columns == 0 || rows == 0 || width == 0 || height == 0 {
        return None;
    }
    let cell_width = width / columns;
    let cell_height = height / rows;
    if cell_width == 0 || cell_height == 0 {
        return None;
    }
    Some(FontSize::new(cell_width, cell_height))
}

/// Parse an `AXON_FONT_SIZE=WxH` override (e.g. "7x15").
fn parse_font_size(value: &str) -> Option<FontSize> {
    let (width, height) = value.trim().split_once(['x', 'X'])?;
    let width: u16 = width.trim().parse().ok()?;
    let height: u16 = height.trim().parse().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(FontSize::new(width, height))
}

fn parse_image_protocol(value: &str) -> Option<ProtocolType> {
    match value.trim().to_ascii_lowercase().as_str() {
        "halfblocks" | "half-blocks" => Some(ProtocolType::Halfblocks),
        "kitty" => Some(ProtocolType::Kitty),
        "sixel" => Some(ProtocolType::Sixel),
        "iterm2" | "iterm" => Some(ProtocolType::Iterm2),
        _ => None,
    }
}

fn detect_image_protocol_from_env() -> Option<ProtocolType> {
    if std::env::var_os("AXON_NO_IMAGE_QUERY").is_some() {
        return None;
    }
    if std::env::var_os("ITERM_SESSION_ID").is_some()
        || std::env::var_os("WEZTERM_EXECUTABLE").is_some()
        || std::env::var("TERM_PROGRAM").is_ok_and(|value| value == "iTerm.app")
    {
        return Some(ProtocolType::Iterm2);
    }
    if !inside_tmux()
        && (std::env::var_os("KITTY_WINDOW_ID").is_some()
            || std::env::var("TERM").is_ok_and(|value| value.contains("kitty")))
    {
        return Some(ProtocolType::Kitty);
    }
    None
}

/// True when running inside a tmux session. Capability queries behave differently
/// here: tmux answers them itself, so a query meant for the outer terminal must be
/// wrapped in tmux passthrough.
pub(crate) fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
        || std::env::var("TERM_PROGRAM").is_ok_and(|value| value == "tmux")
}

/// True when running inside GNU screen (but not tmux, which can share
/// `TERM=screen-*`).  `STY` is screen's authoritative session marker; the
/// `TERM=screen*` check is a fallback for the rare setup where it is unset.
/// screen lacks reliable pixel-graphics passthrough, so callers fall back to
/// halfblocks here.
fn inside_screen() -> bool {
    screen_from_env(
        std::env::var_os("STY").is_some(),
        std::env::var("TERM").ok().as_deref(),
        inside_tmux(),
    )
}

/// Pure core of [`inside_screen`], split out so the precedence rules can be tested
/// without mutating process-global environment variables.
fn screen_from_env(sty_set: bool, term: Option<&str>, inside_tmux: bool) -> bool {
    !inside_tmux && (sty_set || term.is_some_and(|value| value.starts_with("screen")))
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: AxonClient,
    account_filter: Option<Uuid>,
    config: TuiConfig,
    picker: Picker,
) -> anyhow::Result<()> {
    let (live_tx, mut live_rx) = mpsc::unbounded_channel();
    tokio::spawn(websocket_task(client.clone(), live_tx));

    // Whether the terminal's keyboard-enhancement protocol is active (pushed by
    // TerminalGuard). Queried once now, before the input thread starts consuming
    // stdin, so the /editconfig suspend/resume can drop and restore it around the
    // external editor.
    let kbd_enhanced = matches!(supports_keyboard_enhancement(), Ok(true));

    let (key_tx, mut key_rx) = mpsc::unbounded_channel();
    let input_paused = Arc::new(AtomicBool::new(false));
    std::thread::spawn({
        let paused = input_paused.clone();
        move || input_task(key_tx, paused)
    });

    let (lifecycle_tx, mut lifecycle_rx) = mpsc::unbounded_channel();
    let (media_tx, mut media_rx) = mpsc::channel(app::MEDIA_WORKERS * 2);
    let (search_tx, mut search_rx) = mpsc::unbounded_channel();
    let (relations_tx, mut relations_rx) = mpsc::unbounded_channel();
    let (members_tx, mut members_rx) = mpsc::unbounded_channel();
    let (drafts_tx, mut drafts_rx) = mpsc::unbounded_channel();
    let (room_action_tx, mut room_action_rx) = mpsc::unbounded_channel();
    let (bootstrap_tx, mut bootstrap_rx) = mpsc::unbounded_channel();
    let mut app = App::new(client, account_filter, config, picker);
    app.set_lifecycle_sender(lifecycle_tx);
    app.set_media_sender(media_tx);
    app.set_search_sender(search_tx);
    app.set_relations_sender(relations_tx);
    app.set_members_sender(members_tx);
    app.set_drafts_sender(drafts_tx);
    app.set_room_action_sender(room_action_tx);
    app.set_bootstrap_sender(bootstrap_tx);
    app.set_device_id(app::load_or_create_device_id(&app.config_path));
    // Startup runs as spawned stages drained below, so the loop paints and
    // accepts keys immediately instead of after five sequential awaits — which
    // on a server with thousands of rooms looked like a hang (#189). The
    // ordering those stages need (read markers before the first timeline load)
    // is preserved inside `app::bootstrap`.
    app.start_bootstrap();

    let mut tick = time::interval(Duration::from_millis(100));
    let mut next_sixel_inline_refresh = Instant::now() + app::SIXEL_REFRESH_INTERVAL;
    loop {
        terminal.draw(|frame| draw(frame, &mut app))?;

        tokio::select! {
            _ = tick.tick() => {
                let now = Instant::now();
                // Flush settled draft / read-marker changes to the server
                // (M12 debounce).
                app.flush_due_draft_put(now);
                app.flush_due_marker_put(now);
                // Send typing:false once the compose buffer has been idle
                // (ADR 0068 M19a).
                app.flush_due_typing(now);
                // Expire stale inbound typing overlays (M18, ADR 0056).
                app.prune_typing(now);
                // Pull in list titles for rooms scrolling or filtering has just
                // brought on screen. Bounded and idempotent — already-titled
                // rooms are skipped (#189).
                app.sweep_visible_room_titles();
                if inside_tmux()
                    && app.picker.protocol_type() == ProtocolType::Sixel
                    && now >= next_sixel_inline_refresh
                {
                    app.sixel_inline_generation =
                        app.sixel_inline_generation.wrapping_add(1);
                    next_sixel_inline_refresh = now + app::SIXEL_REFRESH_INTERVAL;
                }
                // The preview deadline lives on `app` so opening a preview can
                // reset it; a main-loop local could not see that event, so it
                // kept running between previews (#49).
                if inside_tmux()
                    && app.picker.protocol_type() == ProtocolType::Sixel
                    && app.mode == Mode::Popup(PopupKind::MediaPreview)
                    && now >= app.sixel_preview_refresh_after
                {
                    app.sixel_preview_generation =
                        app.sixel_preview_generation.wrapping_add(1);
                    app.sixel_preview_refresh_after = now + app::SIXEL_REFRESH_INTERVAL;
                }
            }
            Some(event) = key_rx.recv() => {
                let quit = match event {
                    TuiInputEvent::Key(key) => app.handle_key(key).await,
                    TuiInputEvent::Paste(text) => {
                        app.handle_paste(text);
                        false
                    }
                };
                if quit {
                    break;
                }
                // A keystroke or paste may have changed the compose buffer:
                // (re)start the draft debounce if it diverged from the synced
                // draft.
                app.note_draft_activity();
                if app.take_edit_config_request() {
                    // Pause the input thread so it does not compete with the
                    // editor for /dev/tty keystrokes while it has control.
                    input_paused.store(true, Ordering::SeqCst);
                    // Give the thread up to one poll cycle (50 ms) to finish
                    // any in-progress event::read() and see the pause flag.
                    std::thread::sleep(Duration::from_millis(60));
                    while key_rx.try_recv().is_ok() {} // drain any stray events

                    // Suspend the TUI: restore normal terminal state. Drop the
                    // keyboard-enhancement flags and bracketed paste too so the
                    // editor sees ordinary key encoding and paste behavior.
                    disable_keyboard_enhancement(terminal.backend_mut(), kbd_enhanced);
                    let _ = execute!(terminal.backend_mut(), DisableBracketedPaste);
                    disable_raw_mode()?;
                    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                    terminal.show_cursor()?;

                    let result = open_in_editor(&app.config_path);
                    app.apply_editor_result(result);

                    // Re-enter the TUI, restoring keyboard enhancement and
                    // bracketed paste.
                    enable_raw_mode()?;
                    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                    let _ = execute!(terminal.backend_mut(), EnableBracketedPaste);
                    if kbd_enhanced {
                        let _ = execute!(
                            terminal.backend_mut(),
                            PushKeyboardEnhancementFlags(
                                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                            )
                        );
                    }
                    terminal.clear()?;

                    // Drain events the editor may have left in the buffer,
                    // then resume the input thread.
                    while key_rx.try_recv().is_ok() {}
                    input_paused.store(false, Ordering::Release);
                }
            }
            Some(frame) = live_rx.recv() => {
                // The lossy bus may have dropped device_state frames while the
                // socket was down: on every (re)connect, re-read the merged
                // view instead of assuming what was missed (ADR 0048).
                let reconnected = matches!(frame, api::LiveFrame::Connected);
                if app.handle_live_frame(frame) == LiveFrameAction::RefreshRooms {
                    // Coalesced and off the main task: a WS backlog used to run
                    // one full unpaginated room fetch per unknown-room frame,
                    // each re-triggering the per-room title fan-out (#189).
                    app.request_rooms_refresh();
                }
                // The first Connected arrives while startup is still fetching
                // this exact state, so re-reading it then is pure duplicate
                // work; later ones are real reconnects.
                //
                // Keyed on having seen that first frame rather than on startup
                // being finished: a slow server can take long enough to load
                // rooms that the socket drops and reconnects before the
                // DeviceState stage lands, and those reconnects need the
                // re-read exactly as much as later ones do (#210).
                if reconnected && app.note_connected_frame() {
                    app.request_device_state();
                }
            }
            Some(outcome) = bootstrap_rx.recv() => {
                app.handle_bootstrap_outcome(outcome).await;
            }
            Some(outcome) = lifecycle_rx.recv() => {
                app.handle_lifecycle_outcome(outcome).await;
            }
            Some(outcome) = room_action_rx.recv() => {
                app.handle_room_action_outcome(outcome).await;
            }
            Some(result) = media_rx.recv() => {
                app.handle_media_result(result);
            }
            Some(outcome) = search_rx.recv() => {
                app.handle_search_outcome(outcome);
            }
            Some(outcome) = relations_rx.recv() => {
                app.apply_relation_outcome(outcome);
            }
            Some(outcome) = members_rx.recv() => {
                app.apply_members_outcome(outcome);
            }
            Some(outcome) = drafts_rx.recv() => {
                app.handle_draft_outcome(outcome);
            }
        }
    }

    Ok(())
}

/// Launch the user's preferred editor on `path`, blocking until it exits.
/// Respects `$EDITOR`; falls back to `nano` on Unix and `notepad` on Windows.
/// If `$EDITOR` contains arguments (e.g. `"vim -u NONE"`), they are split on
/// whitespace and passed correctly.
fn open_in_editor(path: &std::path::Path) -> io::Result<()> {
    let editor_var = std::env::var("EDITOR").unwrap_or_else(|_| {
        if cfg!(windows) {
            "notepad".to_owned()
        } else {
            "nano".to_owned()
        }
    });
    let mut parts = editor_var.split_whitespace();
    let bin = parts.next().unwrap_or("nano");
    let extra_args: Vec<&str> = parts.collect();
    std::process::Command::new(bin)
        .args(&extra_args)
        .arg(path)
        .status()?;
    Ok(())
}

/// An input-thread event: either an ordinary key press, or a bracketed-paste
/// block (a drag-and-drop drop, or a clipboard paste) delivered as one atomic
/// string rather than a burst of individual key presses.
enum TuiInputEvent {
    Key(KeyEvent),
    Paste(String),
}

/// Keyboard input thread. Uses `event::poll` with a short timeout so the
/// `paused` flag is checked regularly, allowing the main loop to suspend input
/// while an external editor has control of the terminal.
fn input_task(tx: mpsc::UnboundedSender<TuiInputEvent>, paused: Arc<AtomicBool>) {
    loop {
        if paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => {
                // Re-check the pause flag: it may have been set between poll
                // returning and us calling read().
                if paused.load(Ordering::Relaxed) {
                    continue;
                }
                match event::read() {
                    Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        if tx.send(TuiInputEvent::Key(key)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Paste(text)) => {
                        if tx.send(TuiInputEvent::Paste(text)).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            Ok(false) => {}
            Err(_) => break,
        }
    }
}

/// Opt into the terminal's progressive keyboard-enhancement protocol (a.k.a. the
/// kitty keyboard protocol) when supported, so modified keys like Shift+Enter are
/// reported distinctly rather than collapsing to a bare Enter. Returns whether it
/// was enabled, so the caller knows to pop the matching stack entry on exit.
/// Terminals without support (older builds) are left untouched and rely on the
/// Alt+Enter fallback for line breaks.
fn enable_keyboard_enhancement(out: &mut impl io::Write) -> bool {
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok()
    } else {
        false
    }
}

/// Pop the keyboard-enhancement flags pushed by [`enable_keyboard_enhancement`].
/// A no-op when they were never enabled, so an external program (e.g. an editor)
/// or the restored shell never inherits the enhanced encoding.
fn disable_keyboard_enhancement(out: &mut impl io::Write, enabled: bool) {
    if enabled {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    active: bool,
    kbd_enhanced: bool,
}

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        // Ask the terminal to wrap a drag-and-drop or clipboard paste in
        // bracketed-paste markers so it arrives as one `Event::Paste` instead
        // of a burst of individual key events. Best-effort: a terminal that
        // doesn't understand the escape sequence just ignores it.
        let _ = execute!(stdout, EnableBracketedPaste);
        let kbd_enhanced = enable_keyboard_enhancement(&mut stdout);
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self {
            terminal,
            active: true,
            kbd_enhanced,
        })
    }

    fn leave(&mut self) -> anyhow::Result<()> {
        if self.active {
            disable_keyboard_enhancement(self.terminal.backend_mut(), self.kbd_enhanced);
            let _ = execute!(self.terminal.backend_mut(), DisableBracketedPaste);
            disable_raw_mode()?;
            execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
            self.terminal.show_cursor()?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_screen_from_sty_or_term() {
        // STY is screen's authoritative marker.
        assert!(screen_from_env(true, Some("xterm-256color"), false));
        // TERM=screen* as a fallback when STY is unset.
        assert!(screen_from_env(false, Some("screen-256color"), false));
        assert!(screen_from_env(false, Some("screen"), false));
    }

    #[test]
    fn screen_detection_yields_to_tmux() {
        // tmux often runs with TERM=screen-* and sets STY-like state; it must not
        // be misclassified as screen, since tmux has working Sixel passthrough.
        assert!(!screen_from_env(true, Some("screen-256color"), true));
        assert!(!screen_from_env(false, Some("screen-256color"), true));
    }

    #[test]
    fn no_screen_for_plain_terminals() {
        assert!(!screen_from_env(false, Some("xterm-256color"), false));
        assert!(!screen_from_env(false, None, false));
    }

    #[test]
    fn parses_explicit_image_protocol_overrides() {
        assert_eq!(parse_image_protocol("kitty"), Some(ProtocolType::Kitty));
        assert_eq!(parse_image_protocol("SIXEL"), Some(ProtocolType::Sixel));
        assert_eq!(parse_image_protocol("iterm"), Some(ProtocolType::Iterm2));
        assert_eq!(
            parse_image_protocol("half-blocks"),
            Some(ProtocolType::Halfblocks)
        );
        assert_eq!(parse_image_protocol("unknown"), None);
    }

    // FontSize lacks PartialEq upstream, so compare its fields directly.
    fn wh(font_size: Option<FontSize>) -> Option<(u16, u16)> {
        font_size.map(|f| (f.width, f.height))
    }

    #[test]
    fn parses_font_size_override() {
        assert_eq!(wh(parse_font_size("7x15")), Some((7, 15)));
        assert_eq!(wh(parse_font_size(" 10 X 20 ")), Some((10, 20)));
        assert_eq!(wh(parse_font_size("0x20")), None);
        assert_eq!(wh(parse_font_size("7")), None);
        assert_eq!(wh(parse_font_size("axb")), None);
    }

    #[test]
    fn parses_xtwinops_cell_size_response() {
        assert_eq!(wh(parse_cell_size_response(b"\x1b[6;18;9t")), Some((9, 18)));
        assert_eq!(
            wh(parse_cell_size_response(b"noise\x1b[6;20;10t")),
            Some((10, 20))
        );
        assert_eq!(wh(parse_cell_size_response(b"\x1b[6;0;9t")), None);
        assert_eq!(wh(parse_cell_size_response(b"\x1b[4;900;1600t")), None);
        assert_eq!(wh(parse_cell_size_response(b"not a response")), None);
    }

    #[test]
    fn derives_cell_size_from_window_pixels() {
        // 80 cols x 24 rows over a 560x360 px window -> 7x15 cells.
        assert_eq!(wh(font_size_from_window(80, 24, 560, 360)), Some((7, 15)));
        // Terminals that don't report pixel dimensions yield None (fall back to halfblocks).
        assert_eq!(wh(font_size_from_window(80, 24, 0, 0)), None);
        assert_eq!(wh(font_size_from_window(0, 0, 560, 360)), None);
        // Sub-cell pixel size (rounds to 0) is rejected rather than producing a 0px cell.
        assert_eq!(wh(font_size_from_window(600, 24, 560, 360)), None);
    }

    #[test]
    fn detects_sixel_in_da1_response() {
        // xterm in sixel mode: attribute 4 present.
        assert!(sixel_in_da1(b"\x1b[?62;4;6;9;22c"));
        // Attribute 4 in any position.
        assert!(sixel_in_da1(b"\x1b[?4c"));
        assert!(sixel_in_da1(b"\x1b[?1;2;4c"));
    }

    #[test]
    fn rejects_da1_without_sixel() {
        // VT100-style reply, no sixel.
        assert!(!sixel_in_da1(b"\x1b[?1;2c"));
        // "4" must be its own parameter, not a substring (e.g. 14, 64).
        assert!(!sixel_in_da1(b"\x1b[?14;64c"));
        // Garbage / no DA1 block.
        assert!(!sixel_in_da1(b""));
        assert!(!sixel_in_da1(b"not a response"));
        // Unterminated reply.
        assert!(!sixel_in_da1(b"\x1b[?62;4"));
    }
}
