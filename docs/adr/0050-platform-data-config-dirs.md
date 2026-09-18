# ADR 0050 — Platform-convention data / config / cache directories

## Context

Axon's four on-disk locations all defaulted to **current-directory-relative** paths
rooted at `axon-data/` next to wherever the binary was launched:

| Data | Config key | Old default |
|---|---|---|
| Matrix SDK state + crypto (durable) | `sync.data_dir` | `axon-data/sync` |
| Tantivy search index (durable) | `search.index_path` | `axon-data/search` |
| Media LRU cache (disposable) | `media.cache_dir` | `axon-data/media` |
| Config file | `$AXON_CONFIG` / `./axon.toml` | CWD-relative |

That is not standard on any platform and is operationally clunky: state lands wherever
the process happens to be started, an unqualified `axon-data/` tree litters the working
directory, and the config file is only discoverable from an env var or the CWD (issue
#45, raised in review of #38).

## Decision

Default each location to the **OS-standard base directory**, resolved via the
`directories` crate (`ProjectDirs::from("", "", "axon-server")`), while keeping every location
overridable by its existing config key / `AXON_*` env var. The base dirs follow XDG on
Linux, `~/Library` on macOS, and the Known Folders API on Windows.

The project directory is the **binary name** (`axon-server`), matching
`axon-tui`'s `~/.config/axon-tui/config.toml`. An earlier revision of this ADR
used `axon` / `axon.toml`; that layout remains a read-only fallback (see
Amendment below).

### Split data vs. cache vs. config

The three roots are not the same directory, and we map each surface to the one whose
durability contract matches it:

- **Durable state → data dir.** `sync.data_dir` and `search.index_path` hold
  irreplaceable state (crypto stores, historical Megolm keys, the index) and default
  under the platform **data** directory.
- **Disposable cache → cache dir.** `media.cache_dir` is an explicitly bounded,
  re-fetchable convenience (there is deliberately no durable media backend) and defaults
  under the platform **cache** directory — the correct home for content the OS may
  reclaim.
- **Config → config dir.** Config-file discovery gains a third tier: after `$AXON_CONFIG`
  and `./axon.toml`, it looks for `config.toml` in the platform **config** directory,
  then the pre-rename `axon.toml` in the `axon` project directory.

Resolved defaults:

| | Linux (XDG) | macOS | Windows |
|---|---|---|---|
| sync | `~/.local/share/axon-server/sync` | `~/Library/Application Support/axon-server/sync` | `%APPDATA%\axon-server\data\sync` |
| search | `~/.local/share/axon-server/search` | `…/axon-server/search` | `%APPDATA%\axon-server\data\search` |
| media | `~/.cache/axon-server/media` | `~/Library/Caches/axon-server/media` | `%LOCALAPPDATA%\axon-server\cache\media` |
| config | `~/.config/axon-server/config.toml` | `~/Library/Application Support/axon-server/config.toml` | `%APPDATA%\axon-server\config\config.toml` |

### `--config` CLI flag

The `axon` binary gains a global `--config <PATH>` flag (applies to the server and every
subcommand), taking precedence over `$AXON_CONFIG` and the CWD/config-dir discovery. This
makes the config *location* settable without an env var — the second half of the issue's
"configurable config directory" ask; the config *contents* (including the data-dir keys)
were already overridable.

### Hard switch, no legacy fallback

We change the defaults outright rather than auto-detecting a pre-existing `./axon-data`.
Auto-detection would silently prefer a stale CWD tree over the new convention forever and
add a confusing precedence tier. The one fallback retained is for a genuinely homeless
environment (`ProjectDirs::from` returns `None` — no `$HOME` and no passwd entry, e.g. a
stripped-environment container): each default falls back to the old CWD-relative
`axon-data/…` path so the binary still boots.

That hard cut applied to `./axon-data` only. The later `axon` → `axon-server` rename
*does* keep a read-only fallback (see Amendment) because repeating the silent empty-store
failure was not acceptable.

### `directories` over `etcetera`

`etcetera` is already in the tree transitively and can return data/config/cache roots, but
its default app strategy is XDG on macOS; getting the native `~/Library` mapping would
require choosing a different strategy than its CLI default. `directories::ProjectDirs`
matches the exact platform-native table above with one call, including macOS `~/Library`
and Windows Known Folders. The added direct dependency is small and keeps the code aligned
with the ADR's stated conventions.

## Amendment — `axon` → `axon-server` (native packaging)

The original cut from `./axon-data` to platform dirs was a hard switch with no
fallback, and operators who had not pinned paths appeared to have no accounts.
This rename does not repeat that.

- **`axon init` writes only the new path.** `~/.config/axon-server/config.toml`
  (and the macOS/Windows equivalents).
- **Discovery still reads the old path.** After `$AXON_CONFIG` and `./axon.toml`,
  look for the new file, then `~/.config/axon/axon.toml`.
- **Legacy config keeps legacy data/cache defaults** when those keys are omitted,
  so a file that never set `sync.data_dir` continues to point at
  `~/.local/share/axon/sync` rather than an empty `axon-server` tree.
- **No automatic move.** `store_key` plus encrypted SDK state make a botched copy
  unrecoverable. The server logs a warning every boot naming both paths.
- **`./axon.toml` is unchanged** — it is the CWD override for a git checkout, not
  a legacy XDG path.

## Consequences

- **Upgrade impact.** An existing deployment that relied on the `./axon-data` default now
  looks in the platform data/cache dirs and will appear to have no accounts (an empty SDK
  store → re-login, loss of historical Megolm sessions). Operators upgrading across this
  change must either **move** their `axon-data/{sync,search}` (and, if they care to keep
  the cache, `media`) into the new locations, or **pin** the old paths via the
  `sync.data_dir` / `search.index_path` / `media.cache_dir` keys (or the matching
  `AXON_SYNC__DATA_DIR` etc. env vars). This must be called out in the self-hosting guide
  when it is written (M12) and in release notes.
- **Config keys evaluate the environment.** The three path defaults are now computed at
  load time from `$HOME` / the `XDG_*` vars, not fixed constants; the config-loader unit
  tests pin the XDG vars to assert the Linux mapping deterministically. macOS/Windows
  mappings are documented here rather than unit-asserted (CI is Linux-only).
- **`sync.backfill_disk_guard_path`** already derives from `data_dir` when unset, so the
  disk-space valve follows the relocated data dir automatically.
</content>
