//! `axon-demo-tui` — the ADR 0086 TUI demo pilot.
//!
//! ```sh
//! # 1. bring a demo world up and leave it running
//! cargo run -p axon-smoke-local-stack -- up \
//!     --manifest /tmp/demo.json \
//!     --corpus smoke/local-stack/corpus/demo.toml --keep-up
//!
//! # 2. start a screen recorder, then drive the TUI through it
//! cargo run -p axon-smoke-tui --bin axon-demo-tui -- --manifest /tmp/demo.json
//! ```
//!
//! It spawns the real `axon-tui` under a PTY and copies that PTY to stdout
//! **verbatim**, so the developer's terminal draws genuine Sixel/Kitty/iTerm2
//! graphics and a screen recorder captures what the client actually renders.
//! No headless recorder does: `agg` and xterm.js-based tools reduce the TUI's
//! most distinctive output to halfblock approximations.
//!
//! It is not a test and is not a CI gate — recording needs Docker and a real
//! terminal. It is, though, held to the same black-box rule as the smoke
//! harness it shares a package with: it depends on no `axon-*` product crate
//! and only ever spawns the shipped binary.

mod pilot;
mod scenes;
mod term;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context};

const USAGE: &str = "\
Usage: axon-demo-tui --manifest PATH [options]

  --manifest PATH        manifest of a stack brought up with --corpus (required)
  --scene NAME           scene to play; default `tour` (see --list-scenes)
  --list-scenes          print the scene names and exit
  --pace FACTOR          multiply every dwell; default 1.0 (0 = no dwells)
  --timeout SECONDS      per-wait deadline; default 30
  --image-protocol NAME  sixel | kitty | iterm2 | halfblocks
                         default: from this terminal's environment, else sixel
  --font-size WxH        cell size in pixels; default: asked of this terminal
  --keep-dirs            keep the child's scratch config directory
";

struct Args {
    config: pilot::Config,
    list_scenes: bool,
}

fn parse_args() -> anyhow::Result<Option<Args>> {
    let mut manifest: Option<PathBuf> = None;
    let mut scene = "tour".to_owned();
    let mut pace = 1.0f32;
    let mut timeout = Duration::from_secs(30);
    let mut image_protocol: Option<String> = None;
    let mut font_size: Option<(u16, u16)> = None;
    let mut keep_dirs = false;
    let mut list_scenes = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| -> anyhow::Result<String> {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
        };
        match arg.as_str() {
            "--manifest" => manifest = Some(PathBuf::from(value("--manifest")?)),
            "--scene" => scene = value("--scene")?,
            "--list-scenes" => list_scenes = true,
            "--pace" => {
                pace = value("--pace")?
                    .parse()
                    .context("--pace expects a number, e.g. 1.5")?;
                if !pace.is_finite() || pace < 0.0 {
                    bail!("--pace must be zero or greater");
                }
            }
            "--timeout" => {
                let secs: u64 = value("--timeout")?
                    .parse()
                    .context("--timeout expects whole seconds")?;
                timeout = Duration::from_secs(secs.max(5));
            }
            "--image-protocol" => image_protocol = Some(value("--image-protocol")?),
            "--font-size" => {
                let raw = value("--font-size")?;
                font_size = Some(parse_font_size(&raw)?);
            }
            "--keep-dirs" => keep_dirs = true,
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(None);
            }
            other => bail!("unknown argument: {other}\n\n{USAGE}"),
        }
    }

    if list_scenes {
        // Checked before --manifest so the list is available without a stack.
        return Ok(Some(Args {
            config: pilot::Config {
                manifest: PathBuf::new(),
                scene,
                pace,
                timeout,
                image_protocol: String::new(),
                font_size,
                keep_dirs,
            },
            list_scenes,
        }));
    }

    let manifest = manifest.ok_or_else(|| anyhow::anyhow!("--manifest is required\n\n{USAGE}"))?;
    Ok(Some(Args {
        config: pilot::Config {
            manifest,
            scene,
            pace,
            timeout,
            image_protocol: image_protocol.unwrap_or_else(default_image_protocol),
            font_size,
            keep_dirs,
        },
        list_scenes,
    }))
}

fn parse_font_size(raw: &str) -> anyhow::Result<(u16, u16)> {
    let (width, height) = raw
        .trim()
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow::anyhow!("--font-size expects WxH, e.g. 8x16"))?;
    let width: u16 = width.trim().parse().context("--font-size width")?;
    let height: u16 = height.trim().parse().context("--font-size height")?;
    if width == 0 || height == 0 {
        bail!("--font-size must be positive, e.g. 8x16");
    }
    Ok((width, height))
}

/// Guess the protocol from *this* terminal's environment, mirroring the checks
/// `axon-tui` would run for itself.
///
/// The child cannot make this call: its PTY has nobody on the other end to
/// answer a DA1 probe, and the environment it sees is the one we hand it. Sixel
/// is the fallback because it is the widely supported graphics protocol among
/// terminals that advertise nothing; `--image-protocol halfblocks` is the
/// escape hatch when a terminal turns out not to support it.
fn default_image_protocol() -> String {
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term = std::env::var("TERM").unwrap_or_default();
    if std::env::var_os("ITERM_SESSION_ID").is_some()
        || std::env::var_os("WEZTERM_EXECUTABLE").is_some()
        || term_program == "iTerm.app"
    {
        return "iterm2".to_owned();
    }
    // Kitty's protocol does not survive tmux here, matching the client's own
    // check; fall through to Sixel, which tmux passthrough does carry.
    if std::env::var_os("TMUX").is_none()
        && (std::env::var_os("KITTY_WINDOW_ID").is_some() || term.contains("kitty"))
    {
        return "kitty".to_owned();
    }
    "sixel".to_owned()
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => return ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("demo(tui): {err:#}");
            return ExitCode::from(2);
        }
    };

    if args.list_scenes {
        println!("tour  (every scene below, in order)");
        for name in scenes::SCENE_NAMES {
            println!("{name}");
        }
        return ExitCode::SUCCESS;
    }

    // Nothing may print between here and the end of the run: the child owns the
    // screen, and a stray line would be recorded. The log is held and printed
    // after the terminal is restored.
    let mut log = pilot::RunLog::default();
    let result = pilot::run(&args.config, &mut log);
    log.print();

    match result {
        Ok(()) => {
            eprintln!("demo(tui): scene {} complete", args.config.scene);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("demo(tui): scene {} failed: {err:#}", args.config.scene);
            ExitCode::FAILURE
        }
    }
}
