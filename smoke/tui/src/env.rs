//! Binary resolution and the isolated environment each TUI process runs in.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context};

/// Resolve the `axon-tui` binary to drive.
///
/// `AXON_TUI_BIN` overrides everything. Otherwise we build `axon-tui` from the
/// workspace (a no-op when already fresh) and resolve it under the target dir.
pub fn resolve_tui_bin() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("AXON_TUI_BIN") {
        let path = PathBuf::from(path);
        if !path.exists() {
            bail!(
                "AXON_TUI_BIN points at {} which does not exist",
                path.display()
            );
        }
        // Each scenario runs the child in its own temp working directory, so a
        // relative override would fail to resolve. Make it absolute.
        return std::fs::canonicalize(&path)
            .with_context(|| format!("canonicalize AXON_TUI_BIN {}", path.display()));
    }

    eprintln!("smoke: building axon-tui (set AXON_TUI_BIN to skip)…");
    let status = Command::new(env_cargo())
        .args(["build", "-p", "axon-tui"])
        .status()
        .context("run cargo build -p axon-tui")?;
    if !status.success() {
        bail!("cargo build -p axon-tui failed");
    }

    let bin = resolve_workspace_bin(bin_name())?;
    if !bin.exists() {
        bail!("built axon-tui but did not find it at {}", bin.display());
    }
    Ok(bin)
}

pub fn env_cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

fn bin_name() -> &'static str {
    if cfg!(windows) {
        "axon-tui.exe"
    } else {
        "axon-tui"
    }
}

/// Cargo target directory: `CARGO_TARGET_DIR` if set, else `<workspace>/target`.
fn target_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(workspace_root()?.join("target"))
}

pub fn resolve_workspace_bin(name: &str) -> anyhow::Result<PathBuf> {
    Ok(target_dir()?.join("debug").join(name))
}

/// Workspace root: two levels up from this crate (`smoke/tui`).
fn workspace_root() -> anyhow::Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("cannot derive workspace root from {}", manifest.display()))
}

/// The environment every TUI child gets, whoever spawns it: an isolated config
/// directory, a shielded `HOME`, a pinned locale, and no ambient logging.
///
/// `config_home` isolates the config the TUI writes on first run; `home`
/// shields the developer's real home directory.
///
/// Deliberately says nothing about `TERM` or the image protocol. Those are the
/// two settings the smoke harness and the demo pilot must disagree about: the
/// harness pins a synthetic terminal for determinism, while the pilot must
/// describe the *real* terminal its output is being pumped to.
pub fn base_child_env(config_home: &Path, home: &Path) -> Vec<(String, String)> {
    vec![
        (
            "XDG_CONFIG_HOME".to_owned(),
            config_home.to_string_lossy().into_owned(),
        ),
        ("HOME".to_owned(), home.to_string_lossy().into_owned()),
        ("LANG".to_owned(), "C.UTF-8".to_owned()),
        ("LC_ALL".to_owned(), "C.UTF-8".to_owned()),
        // Keep the binary from picking up a developer .env at the working dir.
        ("RUST_LOG".to_owned(), "off".to_owned()),
    ]
}

/// The controlled environment for a *smoke* TUI child process: the shared base
/// plus a pinned `TERM` so the rendered screen is deterministic across
/// machines, and no image-protocol query at all.
pub fn child_env(config_home: &Path, home: &Path) -> Vec<(String, String)> {
    let mut env = base_child_env(config_home, home);
    env.push(("TERM".to_owned(), "xterm-256color".to_owned()));
    // Disable the terminal image-protocol query: from_query_stdio() leaves a
    // background thread blocked on stdin when no response arrives, causing it
    // to race crossterm for injected keystrokes and swallow them silently.
    //
    // The demo pilot must *not* set this (ADR 0086): it wants real graphics, so
    // it declares the protocol and cell size explicitly instead.
    env.push(("AXON_NO_IMAGE_QUERY".to_owned(), "1".to_owned()));
    env
}
