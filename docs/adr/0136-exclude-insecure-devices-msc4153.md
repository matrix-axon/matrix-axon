# ADR 0136 — Exclude non-cross-signed devices (MSC4153)

**Status:** Accepted. Implementation is tracked in [issue 1081](https://github.com/matrix-axon/matrix-axon/issues/1081).

## Context

[MSC4153](https://github.com/matrix-org/matrix-spec-proposals/blob/main/proposals/4153-invisible-crypto.md) ("Exclude non-cross-signed devices") is merged.
It makes cross-signing the basis of device trust, and its two central rules are:

- "clients SHOULD NOT send encrypted to-device messages, such as room keys or secrets (via Secret Sharing), to non-cross-signed devices by default."
  A client can tell the excluded device why with an `m.room_key.withheld` message whose code is `m.unverified`.
- "messages sent from non-cross-signed devices cannot be trusted and SHOULD NOT be displayed to the user."

The MSC also says:

- Users SHOULD have cross-signing keys and Secret Storage, and clients SHOULD encourage cross-signing new and existing devices.
- When a user's own cross-signing keys change, their own devices "MUST alert her to this fact, and prompt her to re-cross-sign those devices."
  When another user's keys change, clients "SHOULD inform him of Alice's key change and SHOULD prevent him from sending an encrypted message to Alice without acknowledging the change", and may treat previously verified users differently.
- Devices that have never uploaded identity keys MUST NOT count as non-cross-signed devices, and SHOULD NOT cause warnings or refused sends.
- "Clients MAY make provisions for encrypted bridges": a client MAY accept a message a bridge bot's Megolm session encrypted on a bridged user's behalf, "provided the session creator's device is cross-signed", and MUST then show a warning on it unless it can check that the bot may encrypt for that user ([MSC4350](https://github.com/matrix-org/matrix-spec-proposals/pull/4350)).
- Because users of clients without cross-signing would be cut off, the two central rules should be delayed "(or make it optional) until most clients have implemented cross-signing."
- Clients may allow exceptions for specific devices for development or testing.

The MSC's "Potential Issues" section quotes the sending rule as "MUST NOT".
The rule's own heading and body say SHOULD NOT, and this ADR follows the normative text.

### Where the rules land in Axon

In Axon the server is the Matrix device.
matrix-sdk runs in `axon-sync`, each `accounts` row is one Matrix account synced through Axon's own device, and the TUI and web clients never hold keys.
So the MSC's client rules are server work, and the clients' share of it is showing the new states.

matrix-sdk 0.19.1 implements both central rules, the way Element's "exclude insecure devices" mode uses them:

- `ClientBuilder::with_room_key_recipient_strategy(CollectStrategy::IdentityBasedStrategy)` shares room keys only with devices their owner cross-signed.
  The SDK withholds keys from the rest with a withheld code.
  It refuses to encrypt at all with `CrossSigningNotSetup` when the account has no cross-signing identity, or with `SendingFromUnverifiedDevice` when Axon's own device is not cross-signed.
  It also refuses with `VerifiedUserChangedIdentity(users)` when a user we had verified has a new identity.
- `ClientBuilder::with_decryption_settings` with `TrustRequirement::CrossSignedOrLegacy` refuses to decrypt messages from devices that are not cross-signed, as `MegolmError::SenderIdentityNotTrusted(level)`.
  It still decrypts sessions received before the SDK recorded who sent them ("legacy" sessions).

Axon sets neither today.
`configure_client_builder` in `crates/axon-sync/src/client.rs` sets only the backup download strategy, so every account shares keys with every device and decrypts messages from anyone.

What Axon already has:

- The persisted `verified` flag for Axon's own device, kept current by the verification watcher ([ADR 0026](0026-recovery-key-endpoint-and-verified-derivation.md)).
- Recovery-key import, interactive SAS for our own devices and other users ([ADR 0027](0027-interactive-sas-verification-boundary-and-contract.md), [ADR 0040](0040-cross-user-sas-verification.md)), and QR sign-in that leaves the device cross-signed ([ADR 0097](0097-matrix-oauth-qr-login-and-device-authorization.md)).
- The device list with `is_cross_signed_by_owner` ([ADR 0060](0060-device-list-discovery-endpoint.md)).
- The per-event sender-trust snapshot and the `verification_violation` live overlay ([ADR 0058](0058-sender-device-trust-and-verification-bundle.md)).

What it lacks:

- **Withheld codes.** An undecryptable event is stored with `content IS NULL` and its Megolm session id ([ADR 0014](0014-redecryption-queue.md)), and the SDK's reason for the failure is dropped.
  Other users' clients that already exclude insecure devices withhold keys from an Axon device that is not cross-signed, and Axon cannot say so.
- **Unverified users' identity changes.** The overlay covers only users we had verified (`has_verification_violation`).
  The SDK reports a changed identity of a user we never verified as a pin violation (`identity_needs_user_approval`), and Axon reports nothing.
- **Typed send failures.** `map_sdk_err` in `crates/axon-sync/src/gateway.rs` turns every SDK error that is not a timeout, `M_FORBIDDEN` or load into a generic upstream error.
- **Creating or resetting cross-signing.** Axon imports existing cross-signing keys from Secret Storage, but cannot create them or reset them.

### The SDK cannot accept bridged messages under either trust requirement

A bridge such as a mautrix bridge encrypts every message in a bridged room with its bot's Megolm session.
The event's sender is a puppet user, or the real user when the bridge double-puppets a message they sent from their phone.
The session belongs to the bot.
matrix-sdk-crypto 0.19.1 reports that as `VerificationLevel::MismatchedSender`, and `check_sender_trust_requirement` refuses it under both `CrossSignedOrLegacy` and `CrossSigned`.
So with the read rule on, every bridged message is refused, even when the bot's own device is cross-signed.
The MSC's bridge provision cannot be built from the SDK's decryption settings alone.

### What production looks like

A survey of the production homeserver on 2026-11-12 found:

- Seven bridge bots that use encryption have no cross-signing keys at all.
  The appservice bots that are cross-signed belong to two other appservices.
- One of the three accounts is in about 460 encrypted rooms that include those bots, about 13 of them active in the previous 30 days.
- Two bots that are not bridges also have no cross-signing keys and share encrypted rooms with two of the accounts.
- The third account shares encrypted rooms with no peer that lacks cross-signing.

Turning both rules on for the first account would therefore break every encrypted bridged room in both directions.
The bots would stop receiving room keys, so nothing that account sends would reach the bridged network, and every bridged message would be refused on arrival.
Turning them on for the third account would change nothing visible.

## Decision

### 1. Two phases of protocol work, and two later phases

**Phase 0** is always on and has no setting.
It reports trust problems that exist whatever the setting: incoming withheld keys, identity changes, and our own identity changing.

**Phase 1** adds the opt-in setting that applies the MSC's two central rules.

**Phase 2** handles bridges and bots, and **Phase 3** lets Axon create and reset cross-signing.
Both depend on work outside this ADR's scope and get their own ADRs.

Each phase ships as a server PR first and then one PR per client, per the one-silo rule.
The server PR changes OpenAPI additively ([ADR 0099](0099-openapi-compatibility-guard.md)), and each client PR updates `docs/client-parity.md`.

### 2. Phase 0: record why a message could not be decrypted

At ingest, at the startup sweep and on redecryption, Axon records the SDK's reason for a decryption failure alongside the event.
The reasons it keeps:

- `missing_key`: the session is unknown and no withheld notice arrived.
- `withheld`, with the withheld code (`m.unverified`, `m.blacklisted`, `m.unauthorised`, `m.unavailable`, `m.no_olm`).
- `sender_not_trusted`, with the SDK's verification level. This only appears once Phase 1 is on.
- `other`, for failures that match none of these.

The event read shapes gain a nullable `utd_reason` for an event without content.
An event withheld with `m.unverified` while Axon's own device is not cross-signed is the case worth a prompt.
Clients can show "this sender's app does not share keys with unverified devices; verify Axon to read new messages", and link to the existing verify and recover flows.

The redecryption queue keeps its triggers.
A withheld or trust-refused event is still retried when a key arrives, because a sender can share the key later.

### 3. Phase 0: identity changes warn, and the user can accept them

Axon reports both kinds of identity change:

- A user we had verified now has a different identity. This is the existing `verification_violation`.
- A user we never verified now has a different identity from the one first seen. This is the SDK's pin violation, new in Axon.

`matrix_sdk::Room::subscribe_to_identity_status_changes` reports both kinds per room, as the room members whose identity needs attention.
Axon reads it into a per-room list of those members, includes the list with the room's details, and pushes changes as a live frame.
Two new account-scoped actions resolve a change:

- **Accept** (`UserIdentity::pin`) accepts the new identity of a user we never verified.
- **Withdraw verification** (`UserIdentity::withdraw_verification`) removes the verified state of a user whose identity changed, so they become an unverified user with an accepted identity.

Re-verifying stays the existing cross-user SAS flow.

Sending is **not** blocked by default.
The MSC says clients SHOULD prevent sending until the change is acknowledged; this ADR deviates from that SHOULD on purpose.
Clients show a warning in the room and in the composer that the user can accept, and a send without accepting still goes through.
The SDK enforces one exception: with Phase 1 on, a send to a room with a previously verified user whose identity changed fails with `VerifiedUserChangedIdentity`.
That case surfaces as a typed error (decision 6), and clients offer "withdraw verification and send" as a single action.
The MSC explicitly allows treating previously verified users differently.

Because Axon holds the acknowledgement on the server, every client of the same Axon account sees it.
That avoids, for Axon's own clients, the MSC's known limitation that acknowledgements do not sync between devices.

### 4. Phase 0: alert when our own identity changes

When the account's own master key changes, Axon reports it explicitly, which the MSC requires.
Today `verified` only flips to `false` in that case, which looks the same as a device that was never verified.
The account read shape gains an `own_identity_changed` state that is set when the master key Axon last saw is replaced, and cleared once Axon's device is cross-signed by the new identity.
Clients prompt the user to verify Axon again or enter the recovery key.

### 5. Phase 1: one setting per Axon account, off by default

The setting is a column on `accounts`, `exclude_insecure_devices BOOLEAN NOT NULL DEFAULT false`, read and written through an account-scoped route.
This ADR does not fix the route's path, body or response; the Phase 1 server PR pins them in OpenAPI.
It belongs to the `accounts` row, which is Axon's own device for one Matrix ID:

- It does not affect the user's other clients of the same Matrix account, just as Element keeps its own setting per device.
- One Axon instance with several Matrix accounts sets each separately.
  That is the point for production, where one account must stay off until bridges work and another can turn it on now.

It is not an instance-wide preference like [ADR 0103](0103-tags-favourites-and-space-order.md)'s, and not a key in `axon.toml`.

Following Element, one setting turns on both rules together: `CollectStrategy::IdentityBasedStrategy` for sending and `TrustRequirement::CrossSignedOrLegacy` for decrypting.
There are no separate sending and receiving settings.
`OnlyTrustedDevices` and strict `CrossSigned` are not offered, because they also cut off users who are cross-signed but whom we have not verified, and sessions received before trust data existed.

Both values are builder settings in matrix-sdk, separate from `EncryptionSettings`.
`configure_client_builder` keeps its `with_encryption_settings(production_encryption_settings())` call, which carries the backup download strategy, and adds `with_room_key_recipient_strategy` and `with_decryption_settings` beside it.
The new calls compose with the existing one and do not replace it.
`configure_client_builder` takes the account's value, every production client path applies it (including the restore path and the OAuth staging client), and its tests cover both values.
Changing the setting rebuilds that account's client through the same stop-and-restart path the lifecycle verbs use, so a change never applies to half a sync cycle.

Any valid bearer may turn the setting off.
[ADR 0109](0109-management-api.md) reserves step-up for credential changes, and argues that a bearer can already read every message.
Turning this setting off exposes no more than that.

Turning it on is a different risk.
It can stop the account from exchanging messages in every room with a member whose devices are not cross-signed, including every encrypted bridged room.
So turning it on also needs a valid bearer, but the server additionally requires an acknowledgement of the impact read (decision 6), so a stray or scripted call cannot turn it on unseen.
It does not need step-up, because it changes no credential.

### 6. Phase 1: turning it on is checked first

The server refuses to turn the setting on (409) while Axon's own device is not cross-signed.
With it on, every encrypted send would otherwise fail with `SendingFromUnverifiedDevice`.

Before the user confirms, an impact read lists, from the account's crypto store, the encrypted rooms that have members with devices that are not cross-signed, and those members.
Bots and bridges, which make up most of production's list, are named in it.
It is the only way a user would find out that bridged rooms are about to stop working.

The impact read also returns an acknowledgement token: a digest of the affected rooms and members it listed.
The request that turns the setting on must carry that token.
The server recomputes the impact and answers 409 if the token is missing or no longer matches, with the current impact in the response so the client can show it again.
So the server, not only a client's UI, ensures that whoever turns the setting on was shown what it breaks.
An account with no affected rooms still sends the token, which then names an empty impact.
Turning the setting off needs no token.

### 7. Phase 1: sends fail with typed errors

Gateway sends map the SDK's recipient-collection errors to a 409 with a stable code, instead of a generic upstream error:

- `cross_signing_not_set_up` and `own_device_not_cross_signed`, which point the user to the verify or recover flow.
- `verified_user_changed_identity`, which carries the affected user ids for decision 3's single action.

These can only occur with the setting on.

### 8. Phase 1: refused messages stay undecryptable, and are retried when trust changes

A message the SDK refuses under `CrossSignedOrLegacy` is stored like any other undecryptable event, with `utd_reason` `sender_not_trusted`.
It has no plaintext, so it is not indexed for search, and like any event without content it raises no message alert ([ADR 0113](0113-remote-push-for-suspended-mobile-clients.md)).

It is retried when its reason may have gone away:

- When the sender's device or identity changes (the SDK's `devices_stream` and `user_identities_stream`), Axon retries the stored events from that sender.
- When the setting is turned off, Axon retries every trust-refused event of the account.

Both retries go through the existing redecryption queue ([ADR 0014](0014-redecryption-queue.md)) and follow its rules: per-event failures are logged and skipped, and the `content IS NULL` guard keeps the backfill idempotent.
What they add:

- **Bounded batches.** Trust-refused rows are selected by `utd_reason` in key order, a bounded batch at a time, never as one account-wide result set.
  The account-wide retry runs on the bulk-maintenance connection, not the hot pools, under the `[database.timeouts]` deadlines.
- **Resumable.** The account-wide retry keeps a durable cursor, in the manner of the startup sweep's `utd_startup_redecrypt_attempted_at` marker, so a crash or restart continues where it stopped instead of starting over or losing the work.
  Turning the setting on again cancels a retry that has not finished, since its rows would only be refused again.
- **Debounced per sender.** Device and identity updates arrive in bursts, and a bridge-heavy account sees many of them.
  Axon collects the affected senders over a short window and retries each sender at most once per window, and only senders with trust-refused rows.

Implementation must check that a trust refusal does not trigger the backup download that `BackupDownloadStrategy::AfterDecryptionFailure` runs for missing keys.
The key is present, so fetching it from backup would be wasted.

### 9. Phase 1: history already decrypted is kept

Turning the setting on does not remove or hide plaintext Axon already stored, including messages from devices that were not cross-signed.
Those messages keep the sender-trust snapshot recorded when they were decrypted, and clients keep showing it.
The setting applies from the moment it is turned on.

### 10. No development or testing exceptions

The MSC allows clients to send keys to, or accept messages from, specific devices that are not cross-signed for development or testing.
Axon does not offer that.

### 11. Phase 2, deferred: bridges and bots

Bridges are left to a later ADR.
It starts with two steps:

- **Operations, outside the code:** cross-sign the bots' devices.
  For bridges, that means turning on the bridge's own self-signing option where its version has one.
  For other bots, it means giving them cross-signing keys or cross-signing their device from another session.
  This also helps the account's Element sessions, whose "exclude insecure devices" mode has the same problem.
- **A spike** on accepting bridged messages under the MSC's bridge provision: a `MismatchedSender` message whose session was created by a cross-signed bridge bot's device, shown with the MSC's warning.
  The candidates are a patched `matrix-sdk-crypto`, a contribution upstream, or enforcing the read rule in Axon on top of the decrypted event's verification state instead of in the SDK.
  Axon already vendors `matrix-sdk-base`, but only for a narrow provenance fix; carrying a patch to `matrix-sdk-crypto`'s trust logic would be a larger and different maintenance commitment, and the spike sizes it.
  Enforcing the rule in Axon would keep untrusted plaintext in Axon's database, which decision 8 avoids, and it would lose the SDK's legacy-session exemption, which the SDK does not expose.

[Issue 1066](https://github.com/matrix-axon/matrix-axon/issues/1066) tracks this phase.
Until its ADR lands, an account that uses encrypted bridges keeps the setting off, and decision 6's impact read says why.

### 12. Phase 3, deferred: creating and resetting cross-signing in Axon

Axon will create cross-signing keys and Secret Storage for an account that has neither, and reset them when the user has lost them, as the MSC recommends.
matrix-sdk has the parts: `bootstrap_cross_signing_if_needed`, `Recovery::enable` and `Recovery::reset_identity`.
A reset needs user-interactive authentication, which is a password for a classic homeserver or an approval URL from the OAuth server.
How Axon carries that through its API, next to the sign-in flows of [ADR 0097](0097-matrix-oauth-qr-login-and-device-authorization.md) and [ADR 0117](0117-matrix-oauth-browser-sign-in.md), is that ADR's decision.
[Issue 1067](https://github.com/matrix-axon/matrix-axon/issues/1067) tracks this phase.
Until then, an account without cross-signing sets it up in another client first.

### 13. Logging and diagnostics

Every phase logs its trust decisions with `tracing` structured fields, and never logs key material, recovery keys or tokens:

- **A decryption failure with a recorded reason** logs `account_id`, `room_id`, `event_id`, `utd_reason`, the withheld code or the SDK's verification level, and the sender's user and device id.
  It logs at debug, because on a busy account these are routine, as other undecryptable events are today.
- **A send refused under the setting** logs `account_id`, `room_id`, the 409 code and, for `verified_user_changed_identity`, the affected user ids, at info.
- **A change of the setting** logs `account_id`, the old and new value, and the number of affected rooms the acknowledged impact named, at info.
  A refused attempt to turn it on logs why: no cross-signing, Axon's device not cross-signed, or a missing or stale token.
- **An identity change or acceptance** logs `account_id`, the user id and the kind (pin or verification violation), at info.
- **The trust-refused retry** logs a summary per batch and per finished run (selected, decrypted, still refused), as the redecryption sweep does.

No new developer-mode switch is added.
These logs sit under Axon's own targets, which the default `[log]` filter keeps at info.
The SDK's own crypto detail is muted by that default filter, and the existing documented way to restore it, adding `matrix_sdk_crypto=debug` to `[log] level`, is enough for debugging trust decisions.

## Consequences

- Phase 0 changes nothing about what Axon sends or decrypts.
  It gives every account explanations it lacks today, and production already needs them: other users' clients can withhold keys from Axon's device now.
- With Phase 1 on, a device the user or a server admin adds without cross-signing it gets no room keys from Axon, and its messages are not shown.
- An account with the setting on cannot send encrypted messages from a device that is not cross-signed.
- A script or client that turns the setting on must first read the impact and send back its token; an unacknowledged request is refused.
  Decision 6 prevents that at the switch, and decisions 4 and 7 cover the identity changing afterwards.
- Users of clients without cross-signing cannot exchange encrypted messages with an account that has the setting on, as the MSC accepts.
- An account in encrypted bridged rooms cannot use the setting until Phase 2.
  On production that is the account that would benefit from it most.
- Unread counts come from the homeserver ([ADR 0070](0070-server-derived-unread-counts.md)), so a refused message still counts as unread although no client can show it.
- A send no longer blocks on an identity change unless the setting is on and the user had verified the person.
  That is weaker than the MSC's SHOULD and is recorded here as a deliberate choice.
- The account-scoped route, the `utd_reason` field, the identity-change list and frame, the two identity actions and the 409 codes are all additive to `/v1/`.

## Alternatives considered

- **One setting for the whole instance**, in `axon.toml` or as a preference.
  It would force the account in bridged rooms and the account without bridges to the same choice.
- **Separate settings for sending and receiving.**
  Element ties them together, and either rule alone leaves half the protection off while still breaking some rooms.
- **Blocking every send on an identity change until it is accepted**, as the MSC recommends.
  Rejected for now in favor of a warning the user can accept, except where the SDK enforces it.
- **Enforcing the read rule in Axon instead of the SDK in Phase 1.**
  It is the only route to bridge support without patching the SDK, but it stores untrusted plaintext and loses the legacy-session exemption.
  It stays a candidate for decision 11's spike.
- **Waiting for Phase 3 before shipping Phase 1.**
  Every production account already has cross-signing, so it can use the setting today; creating cross-signing matters for new accounts and lost keys.
