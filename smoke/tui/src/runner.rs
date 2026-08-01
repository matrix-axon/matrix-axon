//! Sequential scenario runner: owns the run ID, per-scenario isolation, failure
//! artifacts, and the process exit code.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{bail, Context};

use axon_smoke_tui::env::{child_env, resolve_tui_bin};
use axon_smoke_tui::local_stack::{LocalStack, Manifest as LocalStackManifest};
use axon_smoke_tui::pty::PtyDriver;

/// Fixed terminal geometry for the suite.
pub const ROWS: u16 = 30;
pub const COLS: u16 = 100;

/// What a scenario returns: pass/fail plus diagnostics captured from the driver.
pub struct ScenarioOutcome {
    pub result: anyhow::Result<()>,
    pub transcript: Vec<u8>,
    pub final_screen: String,
}

impl ScenarioOutcome {
    /// Capture the driver's transcript and rendered screen alongside `result`.
    pub fn capture(driver: &PtyDriver, result: anyhow::Result<()>) -> Self {
        Self {
            result,
            transcript: driver.transcript(),
            final_screen: driver.screen_text(),
        }
    }
}

type ScenarioFn = for<'a> fn(&'a Ctx) -> Pin<Box<dyn Future<Output = ScenarioOutcome> + Send + 'a>>;

/// Shared context handed to every scenario.
pub struct Ctx {
    pub run_id: String,
    pub bin: PathBuf,
    /// Per-operation wait deadline.
    pub timeout: Duration,
    pub local_stack: Option<LocalStackManifest>,
    base_dir: PathBuf,
    artifacts_dir: PathBuf,
}

/// Isolated directories for a single TUI process.
pub struct ScenarioDirs {
    pub config_home: PathBuf,
    pub home: PathBuf,
    pub cwd: PathBuf,
}

impl Ctx {
    /// Spawn the TUI under a fresh PTY pointed at `base_url`, with a freshly
    /// isolated config/home/working directory for `scenario`.
    pub fn spawn_tui(&self, scenario: &str, base_url: &str) -> anyhow::Result<PtyDriver> {
        self.spawn_tui_with_token(scenario, base_url, None)
    }

    /// Spawn the TUI with an optional Axon bearer token.
    pub fn spawn_tui_with_token(
        &self,
        scenario: &str,
        base_url: &str,
        token: Option<&str>,
    ) -> anyhow::Result<PtyDriver> {
        let dirs = self.scenario_dirs(scenario)?;
        let envs = child_env(&dirs.config_home, &dirs.home);
        let mut args = vec!["--base-url".to_owned(), base_url.to_owned()];
        if let Some(token) = token {
            args.push("--token".to_owned());
            args.push(token.to_owned());
        }
        PtyDriver::spawn(&self.bin, &args, &envs, &dirs.cwd, ROWS, COLS)
    }

    fn scenario_dirs(&self, scenario: &str) -> anyhow::Result<ScenarioDirs> {
        let root = self.base_dir.join(scenario);
        let config_home = root.join("config");
        let home = root.join("home");
        let cwd = root.join("cwd");
        for dir in [&config_home, &home, &cwd] {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(ScenarioDirs {
            config_home,
            home,
            cwd,
        })
    }

    fn write_artifacts(
        &self,
        scenario: &str,
        outcome: &ScenarioOutcome,
    ) -> anyhow::Result<PathBuf> {
        let dir = self.artifacts_dir.join(scenario);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        std::fs::write(dir.join("transcript.raw"), &outcome.transcript)
            .context("write transcript")?;
        std::fs::write(dir.join("final-screen.txt"), &outcome.final_screen)
            .context("write final screen")?;
        Ok(dir)
    }
}

/// Run the requested scenarios; returns the process exit code.
pub async fn run(
    profile: &str,
    filter: Option<&str>,
    manifest_path: Option<PathBuf>,
) -> anyhow::Result<i32> {
    if profile != "true-local" && manifest_path.is_some() {
        bail!("--manifest is only valid with --profile true-local");
    }
    let scenarios: Vec<(&str, ScenarioFn)> = match profile {
        "stub" => vec![
            ("launch_and_quit", |c| {
                Box::pin(crate::scenarios::launch_and_quit(c))
            }),
            ("ctrl_c_exit", |c| {
                Box::pin(crate::scenarios::ctrl_c_exit(c))
            }),
            ("send_round_trip", |c| {
                Box::pin(crate::scenarios::send_round_trip(c))
            }),
            ("border_integrity", |c| {
                Box::pin(crate::scenarios::border_integrity(c))
            }),
            ("scroll_pin_on_relation_refresh", |c| {
                Box::pin(crate::scenarios::scroll_pin_on_relation_refresh(c))
            }),
            ("room_sort_filter_surface", |c| {
                Box::pin(crate::scenarios::room_sort_filter_surface(c))
            }),
        ],
        "true-local" => vec![
            ("true_local_launch", |c| {
                Box::pin(crate::scenarios::true_local_launch(c))
            }),
            ("true_local_send_round_trip", |c| {
                Box::pin(crate::scenarios::true_local_send_round_trip(c))
            }),
            ("true_local_command_surfaces", |c| {
                Box::pin(crate::scenarios::true_local_command_surfaces(c))
            }),
            ("true_local_shortcuts_popup", |c| {
                Box::pin(crate::scenarios::true_local_shortcuts_popup(c))
            }),
            ("true_local_status_commands", |c| {
                Box::pin(crate::scenarios::true_local_status_commands(c))
            }),
            ("true_local_room_navigation", |c| {
                Box::pin(crate::scenarios::true_local_room_navigation(c))
            }),
            ("true_local_send_variants", |c| {
                Box::pin(crate::scenarios::true_local_send_variants(c))
            }),
            ("true_local_relations_render", |c| {
                Box::pin(crate::scenarios::true_local_relations_render(c))
            }),
            ("true_local_react", |c| {
                Box::pin(crate::scenarios::true_local_react(c))
            }),
            ("true_local_thread_panel", |c| {
                Box::pin(crate::scenarios::true_local_thread_panel(c))
            }),
            ("true_local_jump_to_date", |c| {
                Box::pin(crate::scenarios::true_local_jump_to_date(c))
            }),
            ("true_local_room_sort", |c| {
                Box::pin(crate::scenarios::true_local_room_sort(c))
            }),
            ("true_local_room_filter", |c| {
                Box::pin(crate::scenarios::true_local_room_filter(c))
            }),
        ],
        _ => bail!("unknown profile {profile:?}; expected `stub` or `true-local`"),
    };

    let selected: Vec<&(&str, ScenarioFn)> = match filter {
        Some(needle) => {
            let matched: Vec<_> = scenarios
                .iter()
                .filter(|(name, _)| name.contains(needle))
                .collect();
            if matched.is_empty() {
                bail!("--filter {needle:?} matched no scenario");
            }
            matched
        }
        None => scenarios.iter().collect(),
    };

    let run_id = uuid::Uuid::new_v4().to_string();
    let timeout = Duration::from_secs(
        std::env::var("SMOKE_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(20)
            .max(5),
    );
    let base_dir = std::env::temp_dir().join(format!("axon-smoke-tui-{run_id}"));
    let artifacts_dir = artifacts_root()?.join("tui").join(&run_id);
    let keep_up = std::env::var("KEEP_UP").ok().as_deref() == Some("1");
    let local_stack = if let Some(path) = manifest_path {
        Some(LocalStack::attach(path)?)
    } else if profile == "true-local" {
        Some(LocalStack::up(
            &run_id,
            artifacts_dir.join("local-stack.json"),
            keep_up,
        )?)
    } else {
        None
    };
    let owns_stack = local_stack.as_ref().is_some_and(LocalStack::owns_stack);

    let ctx = Ctx {
        run_id: run_id.clone(),
        bin: resolve_tui_bin()?,
        timeout,
        local_stack: local_stack.as_ref().map(|stack| stack.manifest.clone()),
        base_dir: base_dir.clone(),
        artifacts_dir: artifacts_dir.clone(),
    };

    eprintln!(
        "smoke(tui): run {run_id}, profile={profile}, {} scenario(s), binary {}",
        selected.len(),
        ctx.bin.display()
    );

    let mut failures = 0usize;
    for (name, scenario) in &selected {
        eprintln!("smoke(tui): ▶ {name}");
        let outcome = scenario(&ctx).await;
        match &outcome.result {
            Ok(()) => eprintln!("smoke(tui): ✓ {name}"),
            Err(err) => {
                failures += 1;
                eprintln!("smoke(tui): ✗ {name}: {err:#}");
                match ctx.write_artifacts(name, &outcome) {
                    Ok(dir) => eprintln!("smoke(tui):   artifacts → {}", dir.display()),
                    Err(e) => eprintln!("smoke(tui):   (failed to write artifacts: {e:#})"),
                }
            }
        }
    }

    if let Some(stack) = &local_stack {
        if let Err(err) = stack.down() {
            failures += 1;
            eprintln!("smoke(tui): ✗ stack teardown: {err:#}");
        }
    }
    // Best-effort cleanup of the scratch directories.
    let _ = std::fs::remove_dir_all(&base_dir);
    if failures == 0 && !keep_up && owns_stack {
        let _ = std::fs::remove_dir_all(&artifacts_dir);
    }

    eprintln!(
        "smoke(tui): {} passed, {} failed of {}",
        selected.len() - failures,
        failures,
        selected.len()
    );
    Ok(if failures == 0 { 0 } else { 1 })
}

/// `smoke-artifacts/` at the workspace root.
fn artifacts_root() -> anyhow::Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or(manifest);
    Ok(root.join("smoke-artifacts"))
}
