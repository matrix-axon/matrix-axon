# ADR 0098 — Originating megolm key backup, and honest backup state

## Context

Axon can consume server-side megolm key backup.
It cannot originate one.

`POST /v1/accounts/{id}/recover` calls `client.encryption().recovery().recover(recovery_key)`, which imports whatever is in 4S (secret storage).
If the account has cross-signing secrets but no `m.key_backup` account data, recover still self-verifies the device, sets `accounts.verified = true`, and returns 200.
The subsequent UTD sweep then asks `backups().download_room_keys_for_room()` for keys that were never uploaded.

That is expected Matrix E2EE for a brand-new device without backup: Axon cannot decrypt history no device still holding those inbound sessions has uploaded.
What Axon *can* fix is the next rotation.
Login always mints a fresh Matrix device and a fresh SDK crypto store (ADR 0022).
Without Axon creating `m.key_backup` and uploading the sessions it holds, every logout / token death / re-login repeats the loss for keys Axon *did* accumulate.

`RecoveryState::Enabled` is not "history keys imported."
matrix-rust-sdk 0.18 reports `Enabled` when 4S is set up and `all_known_secrets_available()` is true — cross-signing complete locally, and either backups are locally enabled *or* they have been marked disabled in account data.
Axon currently surfaces only `AccountDto.verified` (own-device cross-signing, ADR 0026).
Clients treat a successful recover as "keys recovered."

A production account (`@steve:bostoncoop.net`) hit this: recover imported cross-signing, `verified` became true, and ~21k historical UTDs stayed UTD because no megolm backup existed on the homeserver.
The recover/redecrypt timeout path made it worse: `RedecryptSummary::timed_out()` returned zeros even after selecting tens of thousands of rows.

ADR 0011 already named the consume-only gap ("Revisit if we need accounts without Secure Backup") and never became a milestone.

## Decision

### 1. `recovery().enable_backup()` is not 4S export

`enable_backup()` creates a homeserver backup version and a local decryption key.
It does **not** write `m.megolm_backup.v1` into 4S.
`SecretStore::export_secrets()` is a separate public call that does that.

Axon originates megolm backup only as a **verified** device, and never overwrites someone else's homeserver backup.
The create primitive is `recovery().enable_backup()` and only when `fetch_exists_on_server()` is false, or after deleting a version **this device signed** while `backup_enable_intent` is set and `are_enabled()` is false.
Delete uses ruma `Client::send(delete_backup_version)`, never private `Backups` methods.
`backups().create()` is not called directly and does not 409; a residual fetch-then-POST race is logged, not "fixed" by `disable_and_delete`.

### 2. One SecretStore lifetime; Steve's path uses the existing 4S key

Steve-class recover and enable-with-export do **not** call `Recovery::recover` (that opens 4S, imports, and drops the store).
They:

1. `secret_storage().open_secret_store(recovery_key)` and hold the store until the verb returns.
2. `import_secrets()`.
3. Derive + persist `verified`.
4. Follow the recover or enable tree (`enable_backup` and/or `export_secrets` on **that same store**).
5. Recover skips `wait_for_steady_state` (upload is already kicked; the identity lock still has the UTD sweep).
   Enable drops the identity lock, then `tokio::time::timeout(30s, backups().wait_for_steady_state().into_future())`.
   The named future's `with_delay` is inter-request upload delay, not a wait deadline.

Never log `SecretStore::secret_storage_key()`.
Never put it on the wire.
Never call `recovery().enable()` on this path — that would mint a new recovery key.

### 3. Two trees: recover never 409s a join; enable may

After a successful 4S import, recover auto-enables megolm backup when the homeserver has none, using the recovery key the operator already typed.
If the homeserver already has a backup (seeder / Element X), recover joins (`backup_action=joined` / `already_uploading` / export-only).
If the homeserver has a backup this device cannot decrypt: crash-resume replace only when `backup_enable_intent` and `auth_data` is signed by **this** device; otherwise `backup_action=failed`, still 200.
Never `recover_and_fix_backup`.
Never 409 recover on an existing backup.

`POST /v1/accounts/{account_id}/backup/enable` is the dedicated verb for SAS-then-enable (gossip does not populate 4S) and for export-only / replace retry.
`recovery_key` is required for create, export-only, and crash-resume replace.
Omitting it is kick-upload only: locally enabled → 200 `already_uploading`; else 400.
Unverified device → 409.
Homeserver backup this device is not connected to and did not sign with intent → 409 ("recover first to join").
`RecoveryError::BackupExistsOnServer` maps to 409 on the **enable** verb only.

### 4. `verified` and megolm backup are independent observations

`AccountDto.backup` is a live snapshot from an **async** `BackupStateProvider`, not a Postgres column, not inferred from `RecoveryState::Enabled`.

```text
exists_on_server: Option<bool>   // null if we did not ask or the probe failed
this_device_uploading: bool
backup_state: unknown | creating | enabling | resuming | enabled | downloading | disabling
recovery_state: unknown | enabled | disabled | incomplete
```

Deactivated / deleting / unknown id → unknown snapshot, no homeserver call.
Active GET/list uses local `backups().state()` / `are_enabled()` plus **cached** `exists_on_server()`, bounded 2–5s per account, fanned out with `join_all`.
Decision trees use `fetch_exists_on_server()` (accurate).
GET cache may lag another client's backup creation; enable re-fetches.
PR 1 does not emit `account.backup` WebSocket frames; GET is the reconnect source of truth.

### 5. Recover 200 is "4S import succeeded," not "history keys downloaded"

The recover 200 body flattens `AccountDto` and adds sibling `redecrypt` and `backup_action` so existing TUI/web clients keep reading `data.verified` / `data.account_id`.
Closed enum `backup_action`: `joined` | `enabled` | `export_pending` | `failed` | `already_uploading`.
Enable/export failure after a successful import is still 200 with `failed` / `export_pending`.

### 6. Crash-resume is first-class

`Backups::create()` POSTs `/room_keys/version` before `save_decryption_key`.
A crash between those steps leaves a homeserver version this device cannot decrypt.

New column `accounts.backup_enable_intent BOOLEAN NOT NULL DEFAULT false`.
Set `true` before `enable_backup()`.
Clear only after `export_secrets` succeeds.

Identity of "our crashed create" is **not** `count == 0`.
Replace when all of: intent is true, `are_enabled()` is false, and the current version's `auth_data` signatures include **this** Axon device.
Then `delete_backup_version` for that version and proceed to enable + export.
Someone else's signature → recover 200 `failed`, enable 409; never delete.

Export-only resume: `are_enabled()` and 4S missing `m.megolm_backup.v1` (recovery key required).

### 7. Recover sweep honesty; do not auto-enable backups on the Client builder

`sweep_pending_utds` takes a cancellation token and checks it per room.
On the 30s recover cap, cancel that token and **await** the partial `RedecryptSummary`.
`timed_out(summary)` preserves `selected` / `attempted` / `decrypted`.
The recover sweep stays under the per-identity lock (ADR 0026).
Manual redecrypt uses a 10-minute HTTP cap, drops the identity lock after
`get_or_connect`, and cancels via a child of `AccountTask.cancel` so logout
is not pinned for the sweep.

Do not set `EncryptionSettings.auto_enable_backups = true`.
Login remains a fresh device / fresh crypto store (ADR 0022).
Never call `recovery().disable()`, `reset_key()`, `recover_and_reset()`, `reset_identity()`, or `backups().disable_and_delete()`.
First-device bootstrap that mints a recovery key is a later explicit route, not this path.

## Consequences

**Pros.** Keys this Axon device holds from enable-time onward survive the next logout/login+recover.
Operators can see backup state independently of `verified`.
Steve-class recover auto-enables without rotating the recovery key.
A crashed `create()` does not 409 the originator forever.

**Cons.** Recover auto-enable surprises an import-only operator — mitigated by `backup_action` on the 200.
GET `exists_on_server` can lag another client; enable then 409s — documented staleness.
Pre-Axon history stays UTD unless some other device uploads it; that is success, not a bug.

**Follow-ups (not this ADR's PR).**
TUI `/backup enable` and status copy (landed, #276).
Web backup badge vs "Keys recovered" (landed, #277).
`account.backup` WebSocket frames (deferred: #292).
Optional integration lane: 4S without megolm backup; Axon recover auto-enables; logout; login; recover; post-enable messages decrypt.
Redacted-encrypted events rendered as redactions rather than UTDs.

## Notes

Confirm this number against open PRs/branches before merge — ADR numbers collide across branches the same way migrations do.
If two land on 0098, the later-merged one renumbers.
