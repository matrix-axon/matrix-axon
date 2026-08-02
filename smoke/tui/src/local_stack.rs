use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::env::{env_cargo, resolve_workspace_bin};

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub axon_base_url: String,
    pub axon_bearer_token: String,
    pub rooms: Rooms,
    pub fixtures: Fixtures,
    /// Present only when the stack was brought up with `--corpus` (ADR 0086).
    /// No smoke scenario passes it; `axon-demo-tui` requires it, and addresses
    /// demo fixtures by corpus name — `demo.rooms["trip-photos"]` — rather than
    /// re-deriving room ids from the screen.
    #[serde(default)]
    pub demo: Option<Demo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Demo {
    pub name: String,
    /// The instant the corpus resolved its relative dates against. Scenes that
    /// name a date must count back from this, not from their own `now`: the
    /// stack is meant to outlive the run that seeded it.
    pub seeded_at: DateTime<Utc>,
    pub viewer: DemoViewer,
    pub personas: BTreeMap<String, DemoPersona>,
    pub spaces: BTreeMap<String, DemoSpace>,
    pub rooms: BTreeMap<String, DemoRoom>,
    /// Corpus message `id` → Matrix event ID.
    pub events: BTreeMap<String, String>,
    /// Corpus media path → `mxc://` URI.
    pub media: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DemoViewer {
    pub persona: String,
    pub account: MatrixAccount,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MatrixAccount {
    pub user_id: String,
    pub password: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DemoPersona {
    pub user_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DemoSpace {
    pub room_id: String,
    pub name: String,
    pub children: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DemoRoom {
    pub room_id: String,
    pub name: Option<String>,
    pub direct: bool,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rooms {
    pub general: RoomInfo,
    pub long_timeline: TimelineRoomInfo,
    pub relations: RoomInfo,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoomInfo {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TimelineRoomInfo {
    pub name: String,
    pub jump_dates: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Fixtures {
    pub relations: RelationFixtures,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelationFixtures {
    pub root_event_id: String,
    pub reply_event_id: String,
    pub edit_event_id: String,
    pub peer_reaction_event_id: String,
    pub own_reaction_event_id: String,
    pub formatted_event_id: String,
    pub redacted_event_id: String,
    pub redaction_event_id: String,
    pub thread_root_event_id: String,
    pub thread_member_event_id: String,
}

pub struct LocalStack {
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
    bin: PathBuf,
    owns_stack: bool,
    keep_up: bool,
    torn_down: Cell<bool>,
}

impl LocalStack {
    pub fn up(run_id: &str, manifest_path: PathBuf, keep_up: bool) -> anyhow::Result<Self> {
        let bin = resolve_local_stack_bin()?;
        let mut cmd = Command::new(&bin);
        cmd.args([
            "up",
            "--manifest",
            manifest_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("manifest path is not UTF-8"))?,
            "--run-id",
            run_id,
            "--quiet",
        ]);
        if keep_up {
            cmd.arg("--keep-up");
        }
        let status = cmd.status().context("run axon-smoke-local-stack up")?;
        if !status.success() {
            bail!("axon-smoke-local-stack up failed with {status}");
        }
        let manifest = read_manifest(&manifest_path)?;
        Ok(Self {
            manifest_path,
            manifest,
            bin,
            owns_stack: true,
            keep_up,
            torn_down: Cell::new(false),
        })
    }

    pub fn attach(manifest_path: PathBuf) -> anyhow::Result<Self> {
        let bin = resolve_local_stack_bin()?;
        let manifest = read_manifest(&manifest_path)?;
        Ok(Self {
            manifest_path,
            manifest,
            bin,
            owns_stack: false,
            keep_up: true,
            torn_down: Cell::new(false),
        })
    }

    pub fn owns_stack(&self) -> bool {
        self.owns_stack
    }

    pub fn down(&self) -> anyhow::Result<()> {
        self.torn_down.set(true);
        if !self.owns_stack || self.keep_up {
            eprintln!(
                "smoke(tui): keeping local stack up; manifest {}",
                self.manifest_path.display()
            );
            return Ok(());
        }
        let status = Command::new(&self.bin)
            .args([
                "down",
                "--manifest",
                self.manifest_path.to_string_lossy().as_ref(),
            ])
            .status()
            .context("run axon-smoke-local-stack down")?;
        if !status.success() {
            bail!("axon-smoke-local-stack down failed with {status}");
        }
        Ok(())
    }
}

impl Drop for LocalStack {
    fn drop(&mut self) {
        if self.torn_down.get() {
            return;
        }
        if let Err(err) = self.down() {
            eprintln!("smoke(tui): stack teardown on drop failed: {err:#}");
        }
    }
}

impl Manifest {
    /// Read a manifest written by `axon-smoke-local-stack up`.
    ///
    /// Attaching to a stack this way — rather than through [`LocalStack`] —
    /// neither builds nor tears down anything, which is what `axon-demo-tui`
    /// wants: the recording stack is brought up by hand and outlives the pilot.
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        read_manifest(path)
    }
}

fn read_manifest(path: &Path) -> anyhow::Result<Manifest> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

fn resolve_local_stack_bin() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("AXON_SMOKE_LOCAL_STACK_BIN") {
        return Ok(std::fs::canonicalize(PathBuf::from(path))?);
    }
    eprintln!("smoke: building axon-smoke-local-stack (set AXON_SMOKE_LOCAL_STACK_BIN to skip)…");
    let status = Command::new(env_cargo())
        .args(["build", "-p", "axon-smoke-local-stack"])
        .status()
        .context("cargo build -p axon-smoke-local-stack")?;
    if !status.success() {
        bail!("cargo build -p axon-smoke-local-stack failed");
    }
    let bin = resolve_workspace_bin(if cfg!(windows) {
        "axon-smoke-local-stack.exe"
    } else {
        "axon-smoke-local-stack"
    })?;
    if !bin.exists() {
        bail!("local-stack binary not found at {}", bin.display());
    }
    Ok(bin)
}
