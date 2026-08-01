//! The pilot: spawn `axon-tui` under a PTY, pump its output to the real
//! terminal verbatim, and walk a scene's steps against the rendered screen.

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use axon_smoke_tui::env::{base_child_env, resolve_tui_bin};
use axon_smoke_tui::local_stack::Manifest;
use axon_smoke_tui::pty::{PtyDriver, PtySpawn};

use crate::scenes::{Scene, Step};
use crate::term::{drain_pending_input, repair_terminal, Geometry, RawGuard};

/// Cell size assumed when the terminal reports no pixel dimensions.
///
/// Only reachable when `--font-size` was not given *and* TIOCGWINSZ returned
/// zeros, which is common under tmux and on a few terminals. Graphics come out
/// scaled by this ratio, so a wrong guess is visible rather than silent — the
/// run log says which value was used and where it came from.
const FALLBACK_CELL: (u16, u16) = (8, 16);

/// Rows at the bottom of the screen taken to be the input area: its top border,
/// the buffer/status line, and its bottom border.
///
/// Exactly three. A wider guess reaches up past the message pane's own border
/// and swallows the newest timeline row — which is where a just-sent message
/// lands, so `WaitSent` would wait forever for text it can already see.
const INPUT_ROWS: usize = 3;

pub struct Config {
    pub manifest: PathBuf,
    pub scene: String,
    /// Multiplies every scripted dwell. Waits are unaffected: they are
    /// correctness gates, not pacing.
    pub pace: f32,
    /// Per-wait deadline.
    pub timeout: Duration,
    pub image_protocol: String,
    pub font_size: Option<(u16, u16)>,
    /// Keep the scratch config/home directory instead of removing it.
    pub keep_dirs: bool,
}

/// Lines collected during the run and printed only after the terminal is
/// restored. Nothing may write to stdout or stderr while the child owns the
/// screen — a stray line is baked into the recording.
#[derive(Default)]
pub struct RunLog {
    started: Option<Instant>,
    lines: Vec<String>,
}

impl RunLog {
    fn note(&mut self, message: impl Into<String>) {
        let started = *self.started.get_or_insert_with(Instant::now);
        self.lines.push(format!(
            "  {:>7.2}s  {}",
            started.elapsed().as_secs_f32(),
            message.into()
        ));
    }

    pub fn print(&self) {
        for line in &self.lines {
            eprintln!("{line}");
        }
    }
}

pub fn run(config: &Config, log: &mut RunLog) -> anyhow::Result<()> {
    let manifest = Manifest::read(&config.manifest)
        .with_context(|| format!("read manifest {}", config.manifest.display()))?;
    let demo = manifest.demo.as_ref().ok_or_else(|| {
        anyhow!(
            "manifest {} has no `demo` section: bring the stack up with a corpus first, e.g.\n\
             \n    cargo run -p axon-smoke-local-stack -- up \\\n\
             \x20         --manifest {} \\\n\
             \x20         --corpus smoke/local-stack/corpus/demo.toml --keep-up",
            config.manifest.display(),
            config.manifest.display(),
        )
    })?;

    let scene = Scene::build(&config.scene, demo)?;
    let geometry = Geometry::detect()?;
    let cell = config.font_size.or(geometry.cell).unwrap_or(FALLBACK_CELL);
    let cell_source = if config.font_size.is_some() {
        "--font-size"
    } else {
        geometry.cell_source.label()
    };
    log.note(format!(
        "terminal {}x{} cells, {}x{}px per cell ({}), protocol {}",
        geometry.cols, geometry.rows, cell.0, cell.1, cell_source, config.image_protocol,
    ));
    // A guessed cell size is not an error, it is a quietly wrong-sized image:
    // the sixel is encoded at `cells × cell size` and drawn at those true
    // pixels, so an under-guess renders the picture smaller than the frame
    // around it. Say so, and say how to fix it.
    if config.font_size.is_none() && geometry.cell.is_none() {
        log.note(format!(
            "WARNING: this terminal reported no cell size, so images are scaled by a \
             guessed {}x{}px cell and will not fill their frame. Pass --font-size WxH \
             with your terminal's real cell size.",
            cell.0, cell.1
        ));
    }
    log.note(format!(
        "scene {} ({} steps), viewer {}",
        scene.name,
        scene.steps.len(),
        demo.viewer.account.user_id,
    ));

    let dirs = ScratchDirs::create(config.keep_dirs)?;
    let bin = resolve_tui_bin()?;
    let args = vec![
        "--base-url".to_owned(),
        manifest.axon_base_url.clone(),
        "--token".to_owned(),
        manifest.axon_bearer_token.clone(),
    ];
    let envs = child_env(&dirs, config, cell);

    // Everything from here to `raw.restore()` happens with the terminal owned
    // by the child: no printing, only `log.note`.
    let mut raw = RawGuard::enter()?;
    let driver = PtyDriver::spawn_with(PtySpawn {
        bin: &bin,
        args: &args,
        envs: &envs,
        cwd: &dirs.cwd,
        rows: geometry.rows,
        cols: geometry.cols,
        pixels: Some((geometry.cols * cell.0, geometry.rows * cell.1)),
        tee: Some(Box::new(std::io::stdout())),
    });
    let mut driver = match driver {
        Ok(driver) => driver,
        Err(err) => {
            raw.restore();
            return Err(err);
        }
    };
    // Anything the terminal left in the input queue (a trailing fragment of the
    // cell-size reply, an unsolicited report) must go before keystrokes start
    // flowing to the child, or it is forwarded as if the developer typed it.
    drain_pending_input();
    forward_stdin(&driver);

    let result = drive(&mut driver, &scene, config, log);
    // Grabbed here, not after the quit: `/quit` unwinds the alternate screen,
    // so by then the frame that failed is gone. A scene that fails mid-take is
    // otherwise very hard to reconstruct — the evidence went to the screen.
    let failed_screen = result.is_err().then(|| driver.screen_text());
    let quit = quit(&mut driver, config, log, result.is_err());
    if let Some(screen) = failed_screen.or_else(|| quit.is_err().then(|| driver.screen_text())) {
        match write_artifacts(&scene.name, &screen, &driver) {
            Ok(dir) => log.note(format!("artifacts → {}", dir.display())),
            Err(err) => log.note(format!("failed to write artifacts: {err:#}")),
        }
    }

    raw.restore();
    if quit.is_err() {
        // A child that never left the alternate screen leaves the terminal in
        // it too, since the tee is a straight copy.
        repair_terminal();
    }
    drop(driver);
    dirs.cleanup();

    result.and(quit)
}

/// Walk the scene. Every wait is bounded and fails fast if the child exits.
fn drive(
    driver: &mut PtyDriver,
    scene: &Scene,
    config: &Config,
    log: &mut RunLog,
) -> anyhow::Result<()> {
    driver
        .wait_for_screen_or_exit("the room list to paint", config.timeout, |screen| {
            screen.contains("Rooms")
        })
        .context("first paint")?;
    // The room list can paint before the timeline behind it does; without this
    // the first scripted command races the initial sync.
    driver.wait_for_screen_or_exit("the input line to paint", config.timeout, |screen| {
        screen
            .lines()
            .rev()
            .take(INPUT_ROWS)
            .any(|line| line.contains('>'))
    })?;
    log.note("first paint");

    for step in &scene.steps {
        match step {
            Step::Command(command) => {
                driver.type_text(command)?;
                driver.press_enter()?;
                log.note(format!("command {command}"));
            }
            Step::Type(text) => {
                driver.type_text(text)?;
                log.note(format!("type {text:?}"));
            }
            Step::Key(name, bytes) => {
                driver.send_bytes(bytes)?;
                log.note(format!("key {name}"));
            }
            Step::WaitFor(texts) => {
                let needles = texts.clone();
                driver
                    .wait_for_screen_or_exit(
                        &format!("screen to show {needles:?}"),
                        config.timeout,
                        move |screen| needles.iter().all(|needle| screen.contains(needle)),
                    )
                    .with_context(|| format!("scene {}", scene.name))?;
                log.note(format!("saw {texts:?}"));
            }
            Step::WaitSent(text) => {
                let needle = text.clone();
                driver
                    .wait_for_screen_or_exit(
                        &format!("{needle:?} to render in the timeline"),
                        config.timeout,
                        move |screen| {
                            screen.contains(&needle)
                                && !screen
                                    .lines()
                                    .rev()
                                    .take(INPUT_ROWS)
                                    .any(|line| line.contains(&needle))
                        },
                    )
                    .with_context(|| format!("scene {}", scene.name))?;
                log.note(format!("sent {text:?}"));
            }
            Step::WaitGone(texts) => {
                let needles = texts.clone();
                driver
                    .wait_for_screen_or_exit(
                        &format!("screen to drop {needles:?}"),
                        config.timeout,
                        move |screen| !needles.iter().any(|needle| screen.contains(needle)),
                    )
                    .with_context(|| format!("scene {}", scene.name))?;
                log.note(format!("gone {texts:?}"));
            }
            Step::Dwell(base) => {
                let held = base.mul_f32(config.pace.max(0.0));
                std::thread::sleep(held);
                log.note(format!("dwell {:.1}s", held.as_secs_f32()));
            }
        }
    }
    Ok(())
}

/// End the way a viewer should see it end: `/quit`, the alternate screen
/// unwinding, and the child gone. The smoke runner kills the child instead,
/// which on a real terminal would end a recording on a corrupted screen.
fn quit(
    driver: &mut PtyDriver,
    config: &Config,
    log: &mut RunLog,
    scene_failed: bool,
) -> anyhow::Result<()> {
    if scene_failed {
        // A scene usually fails with something modal still open — a popup, an
        // input prompt — and a popup swallows `/quit` entirely. Unwind first so
        // teardown reports the real failure instead of a teardown timeout on
        // top of it.
        for _ in 0..2 {
            driver.send_bytes(&[0x1b])?;
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    driver.type_text("/quit")?;
    driver.press_enter()?;
    let status = driver
        .wait_for_exit(config.timeout)
        .context("axon-tui did not exit after /quit")?;
    if !status.success() {
        bail!("axon-tui exited with failure status: {status:?}");
    }
    // The leave sequence trails the exit; give the reader thread a moment to
    // see it rather than reporting a restoration failure that did not happen.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !driver.saw_alt_screen_leave() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    if !driver.saw_alt_screen_leave() {
        bail!("axon-tui exited without leaving the alternate screen");
    }
    log.note("clean exit, terminal restored");
    Ok(())
}

/// The rendered screen and the raw transcript, under the same
/// `smoke-artifacts/` root the smoke runner uses (gitignored).
fn write_artifacts(scene: &str, screen: &str, driver: &PtyDriver) -> anyhow::Result<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = root.join("smoke-artifacts").join("demo-tui").join(scene);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(dir.join("final-screen.txt"), screen)?;
    std::fs::write(dir.join("transcript.raw"), driver.transcript())?;
    Ok(dir)
}

/// Copy the developer's keystrokes into the child.
///
/// Detached on purpose: it blocks on stdin and there is nothing to wake it with
/// once the scene is over, so it ends when the process does. Writes fail
/// harmlessly after the child exits.
fn forward_stdin(driver: &PtyDriver) {
    let input = driver.input();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if input.send_bytes(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}

/// The child's environment.
///
/// The two lines that matter are the image ones (ADR 0086). A pilot-owned PTY
/// cannot answer the TUI's DA1 capability probe — nothing is on the other end
/// of it but us — so the protocol is *declared* rather than detected, and
/// `AXON_FONT_SIZE` is declared with it because graphics protocols encode an
/// image at `cells × cell_size` pixels and ask the terminal to draw it at
/// exactly that size. `AXON_NO_IMAGE_QUERY` is deliberately absent: setting it
/// would turn images off, which is the one thing the TUI demo exists to show.
///
/// `TERM` and the multiplexer variables are inherited, not pinned: the child's
/// bytes are going to the developer's real terminal, so it must be described
/// truthfully. That is the opposite of what the smoke harness wants.
fn child_env(dirs: &ScratchDirs, config: &Config, cell: (u16, u16)) -> Vec<(String, String)> {
    let mut envs = base_child_env(&dirs.config_home, &dirs.home);
    envs.push((
        "AXON_IMAGE_PROTOCOL".to_owned(),
        config.image_protocol.clone(),
    ));
    envs.push((
        "AXON_FONT_SIZE".to_owned(),
        format!("{}x{}", cell.0, cell.1),
    ));
    for key in ["TERM", "TERM_PROGRAM", "COLORTERM", "TMUX", "TMUX_PANE"] {
        if let Ok(value) = std::env::var(key) {
            envs.push((key.to_owned(), value));
        }
    }
    envs
}

/// Config/home/working directories for the child, kept out of the developer's
/// real ones. A recording should not depend on — or disturb — local state.
struct ScratchDirs {
    root: PathBuf,
    config_home: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
    keep: bool,
}

impl ScratchDirs {
    fn create(keep: bool) -> anyhow::Result<Self> {
        let root = std::env::temp_dir().join(format!("axon-demo-tui-{}", uuid::Uuid::new_v4()));
        let dirs = Self {
            config_home: root.join("config"),
            home: root.join("home"),
            cwd: root.join("cwd"),
            root,
            keep,
        };
        for dir in [&dirs.config_home, &dirs.home, &dirs.cwd] {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(dirs)
    }

    fn cleanup(&self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
