//! Generates `$OUT_DIR/web_assets.rs`: the built web client (`clients/web/dist`)
//! as a table of `(url path, bytes)` for `src/web.rs` (ADR 0116).
//!
//! Without the `embed-web` feature the table is empty and nothing outside this
//! crate is read, so a plain `cargo build` never needs Node or a built bundle.
//!
//! With it, what a missing bundle means depends on who named the directory:
//!
//! - `AXON_WEB_DIST` is an explicit path, and an explicit path is a promise.
//!   It must be absolute, and a directory there with no `index.html` (a
//!   symlink does not count) fails the build. Release pipelines set it, so an artifact cannot quietly ship
//!   without the web client.
//! - The default, `clients/web/dist` in this checkout, may simply not have been
//!   built, or may be older than the web sources. `build.rs` then runs `pnpm`
//!   itself (see [`autobuild`]), so `cargo build --features embed-web` needs no
//!   separate `pnpm build`. Where it cannot or must not (`CI` is set, `pnpm` is
//!   not installed, `AXON_WEB_AUTOBUILD=0`, or the build fails) a missing
//!   bundle is a warning and an empty table, because the lint gate and CI run
//!   `cargo clippy --all-features` on checkouts that never ran `pnpm`.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=AXON_WEB_DIST");
    println!("cargo:rerun-if-env-changed=AXON_WEB_AUTOBUILD");
    println!("cargo:rerun-if-env-changed=CI");

    let mut table = String::from("pub(crate) static EMBEDDED: &[(&str, &[u8])] = &[\n");
    if let Some(dist) = env::var_os("CARGO_FEATURE_EMBED_WEB").and_then(|_| dist_dir()) {
        // Cargo watches a directory recursively, so a rebuilt bundle is picked up.
        println!("cargo:rerun-if-changed={}", dist.display());
        let mut files = Vec::new();
        collect(&dist, &mut files);
        files.sort();
        for file in &files {
            let relative = file
                .strip_prefix(&dist)
                .expect("collected under dist")
                .components()
                .map(|part| part.as_os_str().to_str().expect("UTF-8 bundle file name"))
                .collect::<Vec<_>>()
                .join("/");
            let source = file.to_str().expect("UTF-8 bundle path");
            writeln!(
                table,
                "    ({:?}, include_bytes!({source:?})),",
                format!("/{relative}")
            )
            .expect("write to String");
        }
    }
    table.push_str("];\n");

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("web_assets.rs");
    fs::write(&out, table).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
}

/// The bundle to embed, or `None` when the default location has not been built.
fn dist_dir() -> Option<PathBuf> {
    let dir = match env::var_os("AXON_WEB_DIST") {
        Some(explicit) => {
            let dir = PathBuf::from(explicit);
            // Cargo runs this script from the package directory, not from where
            // `cargo` was invoked, so a relative path would name the wrong place.
            assert!(
                dir.is_absolute(),
                "AXON_WEB_DIST must be an absolute path, and {} is not \
                 (try AXON_WEB_DIST=\"$PWD/{}\").",
                dir.display(),
                dir.display()
            );
            assert!(
                has_index(&dir),
                "AXON_WEB_DIST names {}, which has no index.html that is a regular file \
                 (symbolic links are not embedded). \
                 Point it at a built web client (`pnpm --dir clients/web build` writes clients/web/dist).",
                dir.display()
            );
            dir
        }
        None => {
            let dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
                .join("../../clients/web/dist");
            // Watched even when absent, so building the bundle later reruns this.
            println!("cargo:rerun-if-changed={}", dir.display());
            autobuild(&dir);
            if !has_index(&dir) {
                println!(
                    "cargo:warning=`embed-web` is on but clients/web/dist is not built, so this \
                     binary carries no web client. Run `pnpm --dir clients/web install && \
                     pnpm --dir clients/web build`, or set AXON_WEB_DIST to require a bundle."
                );
                return None;
            }
            dir
        }
    };
    Some(
        dir.canonicalize()
            .unwrap_or_else(|e| panic!("resolving {}: {e}", dir.display())),
    )
}

/// Inputs to `pnpm build`, relative to `clients/web`. A change to any of them
/// makes the bundle stale; `build.rs` also reruns on a change to any of them.
const WEB_INPUTS: &[&str] = &[
    "src",
    "public",
    "index.html",
    "package.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "vite.config.ts",
    "tsconfig.json",
    "tsconfig.app.json",
    "tsconfig.node.json",
];

/// Builds `clients/web` into `dist` when the bundle is missing or older than
/// its inputs, so a `cargo build --features embed-web` never embeds a stale or
/// absent client. Only the default location is built: `AXON_WEB_DIST` names a
/// bundle somebody else is responsible for.
///
/// Every way this can fail to build is a warning, not an error. The caller
/// then embeds whatever `dist` holds (nothing, if it was never built), which is
/// what a checkout without Node got before this existed.
fn autobuild(dist: &Path) {
    let web = dist.parent().expect("dist has a parent").to_path_buf();
    for input in WEB_INPUTS {
        // Watched even when absent, and recursively for directories.
        println!("cargo:rerun-if-changed={}", web.join(input).display());
    }
    if env::var_os("CI").is_some() || env::var("AXON_WEB_AUTOBUILD").is_ok_and(|v| v == "0") {
        return;
    }
    let built = fs::metadata(dist.join("index.html")).and_then(|meta| meta.modified());
    let newest = WEB_INPUTS
        .iter()
        .filter_map(|input| newest_mtime(&web.join(input)))
        .max();
    let stale = match (&built, newest) {
        (Ok(built), Some(newest)) => newest > *built,
        // No bundle at all, or nothing to compare against.
        (Err(_), _) => true,
        (Ok(_), None) => false,
    };
    if !stale {
        return;
    }
    if !web.join("package.json").is_file() {
        return;
    }
    let pnpm = |args: &[&str]| {
        Command::new("pnpm")
            .arg("--dir")
            .arg(&web)
            .args(args)
            .status()
    };
    // Without pnpm there is nothing to try: the caller's "not built" warning
    // already says how to build by hand.
    match Command::new("pnpm").arg("--version").output() {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {}
    }
    println!("cargo:warning=building the web client for `embed-web` (pnpm build)");
    // `pnpm build` fails loudly enough when `node_modules` is out of date, and
    // an offline install fails slowly, so install only a checkout that has none.
    let ok = |status: std::io::Result<std::process::ExitStatus>| status.is_ok_and(|s| s.success());
    if (web.join("node_modules").is_dir() || ok(pnpm(&["install", "--frozen-lockfile"])))
        && ok(pnpm(&["build"]))
    {
        return;
    }
    println!(
        "cargo:warning=building the web client failed, so `embed-web` embeds whatever \
         clients/web/dist already holds. Build it by hand to see the error \
         (`pnpm --dir clients/web build`), or set AXON_WEB_AUTOBUILD=0 to stop trying."
    );
}

/// The newest modification time of `path`, or of anything under it.
fn newest_mtime(path: &Path) -> Option<SystemTime> {
    let meta = fs::symlink_metadata(path).ok()?;
    let own = meta.modified().ok();
    if !meta.is_dir() {
        return own;
    }
    let children = fs::read_dir(path).ok()?.filter_map(|entry| {
        let entry = entry.ok()?;
        newest_mtime(&entry.path())
    });
    children.chain(own).max()
}

/// Whether `dir` holds an `index.html` that [`collect`] will embed. A symlink
/// does not count: `collect` skips it, and a bundle without its entry point is
/// no web client at all.
fn has_index(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join("index.html")).is_ok_and(|meta| meta.is_file())
}

/// Every regular file under `dir`. Dotfiles and symlinks are left out: a bundle
/// has neither, and what is collected is served to anyone who asks, so a stray
/// `.env` or a link out of the directory must not ride along.
fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        // `DirEntry::file_type` does not follow symlinks.
        let kind = entry
            .file_type()
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        if kind.is_dir() {
            collect(&path, files);
        } else if kind.is_file() {
            files.push(path);
        }
    }
}
