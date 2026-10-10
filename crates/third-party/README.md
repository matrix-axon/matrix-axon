# Temporary Matrix SDK member-count patch

Axon vendors only `matrix-sdk-base` 0.19.1, from upstream revision `b18166c68bb958a21f0bca8b2d8320cb53583362`.
The crates.io release archive SHA-256 is `02d98114e35b09143eb0db0c1679176d56d9bcddd141546d7de272a87e6dc94e`.
The SDK, SDK UI, and base dependency are pinned to 0.19.1; Cargo's root patch selects this source for all consumers.
This directory is a server dependency, excluded from workspace membership.

`member-count-provenance.patch` contains the complete changes relative to that release.
Only `src/room/room_info.rs`, `src/store/migration_helpers.rs`, and `src/response_processors/room/msc4186/mod.rs` differ from the published crate.
The upstream license and copyright notices are preserved.
Registry bookkeeping, the nested Cargo lockfile, and the upstream wasm runner configuration are omitted.

The additive API exposes per-field server-summary availability and the local timestamp of an observed membership transition or initial room replacement.
An absent field retains a known value within the same membership epoch; changing membership or receiving an initial room replacement invalidates both fields.
Legacy caches have no availability evidence and deserialize as unknown, including through the older room-info migration helper.
The SDK's existing numeric getters retain their behavior for other consumers.
Axon publishes a count pair only when both fields are known and joined is positive.
A server-reported zero joined count actively clears the previous Axon pair, even if the invited count is absent.
A positive transition invalidates the previous Axon observation even if the new joined summary is incomplete.
Unknown cold caches do not erase existing timestamped Axon observations.
The SDK transition timestamp establishes that invalidation occurred; Axon's persistence ordering captures PostgreSQL time before reading the SDK snapshot, sharing the leave trigger's clock.

The regression tests in `crates/axon-sync/src/member_counts.rs` exercise the actual patched MSC4186 processor, missing fields, explicit zero, leave/rejoin, same-epoch deltas, initial replacements, serialization, legacy cache fallback, and persistent SQLite restart.
Run `cargo test -p axon-sync --lib member_counts` and the PostgreSQL-gated version documented in `docs/room-metadata.md`.
Upstream serialization and migration regressions can also run with `cargo test --manifest-path crates/third-party/matrix-sdk-base/Cargo.toml --lib room_info --no-default-features`; its generated nested lockfile is temporary and should not be committed.

Upstream issue [2010](https://github.com/matrix-org/matrix-rust-sdk/issues/2010) concerns the related count-validity contract, but is not a tracker for this exact patch.
No exact upstream issue was found when this patch was prepared.
Remove the vendored dependency and patch after a released SDK exposes equivalent provenance; verify omitted-field, legacy-cache, membership-transition, and initial-replacement behavior before upgrading.
