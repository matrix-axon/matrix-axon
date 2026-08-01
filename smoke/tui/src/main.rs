//! `axon-smoke-tui` — black-box PTY smoke harness for the `axon-tui` binary.
//!
//! ```sh
//! cargo run -p axon-smoke-tui -- --profile stub [--filter NAME]
//! cargo run -p axon-smoke-tui -- --profile true-local [--filter NAME] [--manifest PATH]
//! ```
//!
//! It spawns the real `axon-tui` under a pseudo-terminal, points it at an
//! in-process Axum stub of the Axon `/v1/` API, and asserts on the rendered
//! terminal screen and the stub's request journal. It depends on no `axon-*`
//! crate; all wire types are handwritten from the `openapi/` contract.

mod runner;
mod scenarios;
mod stub;
mod wire;

use std::path::PathBuf;
use std::process::ExitCode;

struct Args {
    profile: String,
    filter: Option<String>,
    manifest: Option<PathBuf>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut profile = None;
    let mut filter = None;
    let mut manifest = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => {
                profile = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--profile requires a value"))?,
                );
            }
            "--filter" => {
                filter = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--filter requires a value"))?,
                );
            }
            "--manifest" => {
                manifest =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        anyhow::anyhow!("--manifest requires a value")
                    })?));
            }
            "--help" | "-h" => {
                println!(
                    "Usage: axon-smoke-tui --profile stub|true-local [--filter NAME] [--manifest PATH]"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    Ok(Args {
        profile: profile.unwrap_or_else(|| "stub".to_owned()),
        filter,
        manifest,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("smoke(tui): {err:#}");
            return ExitCode::from(2);
        }
    };

    match runner::run(&args.profile, args.filter.as_deref(), args.manifest).await {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("smoke(tui): fatal: {err:#}");
            ExitCode::from(2)
        }
    }
}
