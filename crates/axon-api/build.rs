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
//!   built. That is a warning and an empty table, because the lint gate and CI
//!   run `cargo clippy --all-features` on checkouts that never ran `pnpm`.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=AXON_WEB_DIST");

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
