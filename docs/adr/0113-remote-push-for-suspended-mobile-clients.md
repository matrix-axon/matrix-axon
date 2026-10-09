# ADR 0113 — Remote push for suspended mobile clients

**Status:** Accepted.
This records the design for #603 and adds no code.
#604 implements the server half.
#605 implements the client half, and it waits until #604 has landed.

## Context

Local notifications (#606) post only while the Axon process is running.
The page hears `timeline.event` on `/v1/ws` and asks the operating system to show a toast.
A phone the OS has suspended never makes that call, because the socket does not survive backgrounding.
ADR 0102 names that gap and leaves the push router for a later ADR.
This is that ADR.

The server already holds decrypted events.
A Matrix homeserver pusher usually does not, which is why encrypted rooms elsewhere get an empty alert.
Axon can put the sender, the room, and the body into a push.
The default alert carries none of them.

What already exists, and what this design uses:

- ADR 0070 stores `notification_count` and `highlight_count` per room.
  Those counts are the SDK's read-receipt push rules.
  Axon does not reimplement mute, mentions-only, or keywords.
- The live bus already carries the decrypted event (`LiveEvent`, including `body`, `sender`, and `relates_to`) and the count frame (`UnreadCountsChanged`).
- Local toasts decide their text in `messageNotificationBody`: the room name as the title, `sender: body` clipped at 140 characters, and `@localpart` when no display name is cached.
- iOS and iPadOS are one Apple build, bundle id `org.matrixaxon.axon`.
  Android uses the same identifier as its package name.
  The message channel #606 creates is `messages-v2`.
- Bearer tokens are instance-scoped.
  One human, every Matrix account, one `tokens` row per client session.
  Revocation sets `revoked_at`.
  It does not delete the row.
- Sign in with Apple already stores a `.p8` key as `private_key` or `private_key_path`, and `Debug` redacts both.
  Push uses the same shape and a different key.
  The OAuth key is not the APNs key.

#602 keeps desktop on local toasts, keeps Windows off any push service, and leaves Web Push for a later cut of the same design.

## Decision

Remote push wakes a client the OS has suspended.
It is an alert the system shows without running the process.
It is not a second copy of the local toast, and it is not a silent wake.

### Who is woken

| Platform | Wake |
| --- | --- |
| iOS and iPadOS | APNs, one registration for the shared build |
| Android | FCM |
| macOS | Local notifications from the running process. A quit Mac app is not woken. |
| Windows | Local notifications from the running process. This design has no Windows push service. |
| Linux desktop | Local notifications from the running process. |
| Browser | No remote push in this cut. Web Push is a later provider. |

macOS stays local on purpose.
Suspension on a phone is the OS taking the process away.
Quitting the Mac app is the user closing it, and the running-app toasts plus System Settings already cover the desktop case.
The same Apple key could address a Mac.
This ADR declines that.

### When a push is sent

The router wakes a device only when ADR 0070's `notification_count` or `highlight_count` for a room rises, and that rise belongs to a newly stored `m.room.message`.

The message is eligible when all of these hold:

- `event_type` is `m.room.message`.
- `content` is present.
  A still-undecryptable event does not wake anyone.
  Re-decryption does not re-emit a live frame, so a later back-fill does not send the push the UTD skipped.
- The sender is not the account's own user id.
- `relates_to.rel_type` is not `m.replace`.
  An edit does not wake, including an edit of a thread reply.
- A thread reply (`m.thread`) does wake.
- `m.notice` is not special-cased.
- The plaintext body is non-blank after whitespace collapse.

The router does not parse Matrix push rules itself.
The count rise is that decision, and it is already the user's policy.
The SDK evaluates the account's `m.push_rules` into push actions, then counts an action that notifies as `notification_count` and an action with the highlight tweak as `highlight_count`.
ADR 0070 stores those two numbers.
A room another client has muted, or a ruleset that notifies only on mentions and keywords, produces no count rise for the messages it suppressed, so this router does not wake for them.
A count that moves with no eligible message in hand does not wake anyone: receipt reconciliation and membership noise are not alerts.

### Push rules are the future customization

The Matrix Client-Server push-rules module already covers the customization this design expects later.
`m.push_rules` is global account data, one ruleset per account, evaluated in this order: override, content, room, sender, underride.
The first match wins.

The shapes a later Axon setting would write are the ones Element already writes, and the ones `matrix-sdk`'s `RoomNotificationMode` already reads:

- All messages.
  The underride message rule notifies, or a room rule for that room notifies.
- Mentions and keywords only.
  A room rule suppresses ordinary messages.
  Mention overrides (`.m.rule.is_user_mention`, `.m.rule.is_room_mention`) and content keyword rules sit above it, so they still notify and still highlight.
- Mute.
  An override that matches the room and does not notify, which also suppresses mentions.
- Keywords and particular senders.
  Content rules and sender rules, already in the same ruleset.

The account default can differ for a direct chat and for an encrypted room, because those are different underride rules.
A per-room choice overrides that default.

v1 does not ship a screen for any of this.
It also does not add a column, a query parameter, or a second flag on `push_registrations`.
Whatever rules are already on the account, including rules set from another Matrix client, apply on the first implementation because they already apply to the counts.

A later editor writes `m.push_rules` through the homeserver, the same account data other clients read.
It does not tell the router directly, and it does not store a private Axon mute list.
The router keeps waking on a `notification_count` rise.
Mentions-only is not implemented by switching the router over to `highlight_count`.
That would be a second copy of the ruleset.
A mention is a notify action plus a highlight tweak, so the notification count already moves for it.

Disclosure stays a different knob.
It chooses what the banner may say.
It does not choose which rooms wake, and a mention does not raise a device's disclosure level.

Two limits are deliberate, so a later change does not have to break the route:

- The ruleset is per account, shared by every device and by any homeserver pusher.
  A phone that should stay mentions-only while a tablet gets every message is not a Matrix push rule.
  If Axon ever needs that, it is an optional filter on the registration, applied after the count rise, and it is a later additive field.
  This ADR does not add the column.
- v1 only wakes for an eligible `m.room.message`.
  Push rules can also notify for an invite or a call.
  A count rise for an event class this list does not name is a no-wake until a later change adds that class.
  Adding it is an extension of the eligibility list, not a new policy engine.

The count can be held back while the SDK has unmatched receipts (ADR 0070).
The router keeps the latest eligible message per account and room in memory, and sends it if the rise is released while it is still held.
The slot is dropped, unsent, at the unread resweep interval (five minutes).
The slot is not durable.
A crash loses at most the wakes in flight, and the next eligible message wakes the device.

The router does not consult WebSocket presence.
A suspended phone can leave a socket the server still considers open, and that is the case this push exists to cover.
Skipping delivery while a bearer looks connected would recreate the gap.

### What the alert may say

Disclosure is a per-device setting stored on the registration.
The client's default, and the value it sends until the user picks otherwise, is `activity`.

| `disclosure` | Title | Body |
| --- | --- | --- |
| `activity` | `Axon` | `New activity` |
| `sender_and_room` | Cached room name, or `New message` | Cached sender display name, or `@localpart` |
| `preview` | Cached room name, or `New message` | The same string as `messageNotificationBody`: `sender: body`, whitespace collapsed, clipped at 140 characters |

Names come from state Axon already cached.
The router does not fetch a profile or a room name over the network in order to send.
A missing name uses the fallback in the table.
A failed local read uses the same fallback and still sends.

The data payload always carries routing fields, as strings:

- `account_id`
- `room_id`
- `event_id`
- `thread_root_id`, omitted when the message is not a thread reply

#605 maps those onto the existing tap target (`accountId`, `roomId`, `eventId`, `threadRootId`).
The alert text for `activity` does not include them.
Apple and Google can still read them.
A tap opens that room, including the thread, without a round trip.
The payload carries no badge count.
The icon badge stays the local total from #607.

APNs sends a visible alert.
`apns-push-type` is `alert` and `apns-priority` is 10.
There is no `content-available` flag.
FCM HTTP v1 sends a notification message plus that data map, with `android.priority` set to `HIGH` and `android.notification.channel_id` set to `messages-v2`.
FCM data values are strings.
The data keys above are not FCM-reserved names.

The title and the body are each clipped at 140 characters before they are placed in the payload.
A payload that would still exceed 4096 bytes is sent as the `activity` alert instead.
If that too would exceed the limit, the wake is dropped and logged.
The router does not send an oversized payload.

A burst is coalesced per room.
The coalesce value is the first 32 hex characters of the SHA-256 of the room id, which fits APNs' 64-byte `apns-collapse-id` limit.
A room id itself can be longer than that header.
APNs is asked to coalesce with that value.
Android is asked to coalesce with `android.notification.tag` set to the same value.
An FCM notification message ignores `collapse_key`, and FCM keeps a single stored notification per app while the device is offline, so a phone that is offline receives the latest alert rather than one per room.
Both requests are best-effort.
Several banners from one burst are acceptable.

### Registration

Two additive routes, both behind the existing bearer gate:

- `PUT /v1/push/registrations`
- `DELETE /v1/push/registrations/{id}`

`PUT` body:

- `provider`: `apns` or `fcm`.
  Any other value, including `webpush`, is `400` with code `push_provider_unsupported`.
- `token`: the APNs device token or the FCM registration token.
  Empty is rejected.
  Longer than 4096 bytes is rejected.
  The cap is checked before the row is written.
- `disclosure`: `activity`, `sender_and_room`, or `preview`.
  Required, so an omitted field cannot land on `preview`.
- `environment`: `sandbox` or `production`, required for `apns` and omitted for `fcm`.

`PUT` is an upsert on `(provider, token)`.
A repeat with the same pair updates `disclosure`, `environment`, and the owning bearer, and returns the same row.
The response is the row's `id`, `provider`, `disclosure`, and `environment`.
It does not echo the token.

`DELETE` removes the row when the calling bearer owns that `id`.
An id that is absent, or that belongs to another bearer, also answers `204`, so the route is idempotent and does not reveal other devices.
A client that has forgotten the id calls `PUT` again and deletes the returned id.

The row lives in `push_registrations`:

- `id` UUID primary key.
- `token_id` UUID, the `tokens.id` of the bearer that wrote it.
- `provider`, `token`, `environment`, `disclosure`.
- `created_at` and `updated_at`, the latter maintained by the shared `updated_at` trigger.
- Unique `(provider, token)`.

There is no second "notifications enabled" flag.
No row means this device is not woken.
Turning message notifications off deletes the registration.
Turning them on registers at the device's current disclosure.
Changing disclosure is another `PUT` of the same token.
Desktop and the browser do not register, and they do not show the disclosure control.
Their local toast already shows the preview, and that text never leaves the device.

One row covers every Matrix account on the instance.
The payload's `account_id` says which one the tap opens.
Signing out of a single Matrix account does not delete the row.
The client deletes it when message notifications are turned off, and when the Axon session that owns the bearer ends.

A failed `DELETE` leaves the local setting off and retries.
The client does not turn the setting back on to match a row the server still has.

Revoking a bearer deletes that bearer's registrations in the same transaction that sets `revoked_at`.
Boot also deletes registrations whose bearer is already revoked, so a crash between the two writes cannot leave a live route.
The foreign key uses `ON DELETE CASCADE` for a token row that is actually removed.
Revocation itself is not a delete, which is why the transaction and the boot pass both exist.

`GET /v1/status` gains a `push` object, `{ "apns": bool, "fcm": bool }`, meaning that provider's key material is configured.
It does not mean a device is registered.
The field is additive.

Registration succeeds when the provider is unconfigured.
The token write is the client's, and it has to converge whether or not the operator has pasted a key.
Delivery then skips that provider.

### Where secrets live

Provider credentials live in config, not in the database and not in the repo.

```toml
[push.apns]
team_id = ""
key_id = ""
private_key = ""          # PEM of the APNs auth key, or
private_key_path = ""     # a bounded private file. One of the two.
bundle_id = "org.matrixaxon.axon"

[push.fcm]
project_id = ""
client_email = ""
private_key = ""          # service-account PEM, or
private_key_path = ""
package_name = "org.matrixaxon.axon"
```

Figment already maps these to `AXON_PUSH__APNS__PRIVATE_KEY` and the matching names for the other fields.
`Debug` redacts `private_key` and `private_key_path` the way `AppleOauthConfig` does.
APNs uses token auth against `api.push.apple.com` or `api.sandbox.push.apple.com`.
FCM uses the HTTP v1 send endpoint with a service-account access token.
Neither the `.p8` key, the service-account key, nor that access token is logged.

The device token is stored as plaintext in `push_registrations.token`.
It is not a Matrix credential: a copy cannot read the account, and sending with it also requires the provider key, which is outside the database.
`pgp_sym_encrypt` is the wrong fit here anyway.
It is non-deterministic, and the idempotent upsert has to find the row by the token value.
Logs, tracing fields, error `Display`, and the `PUT` response identify a registration by its UUID.
They do not include the token, the alert body, the data payload, or the provider authorization header.
These two routes do not log their request bodies.
A provider failure records the HTTP status and, when the body parses as one, APNs `reason` or FCM `error.status`.
It does not record the raw response body.

An APNs `410` with reason `Unregistered`, or an APNs `400` with reason `BadDeviceToken`, deletes that registration.
Any other APNs `400` does not.
`PayloadTooLarge` and `BadTopic` are our bug, not a dead device.
FCM status `UNREGISTERED` (HTTP 404) deletes that registration.
FCM `INVALID_ARGUMENT` does not: the same status covers a bad token and a bad payload, and a payload bug must not wipe the device.
A `403`, including FCM `SENDER_ID_MISMATCH`, does not delete.
A wrong server key must not wipe every device.
`429` and FCM `QUOTA_EXCEEDED` do not delete and do not retry inside the sync loop.

### Delivery

The sync loop hands the router a wake with `try_send` on a bounded channel of 64.
A full channel drops the new wake and logs the registration id, the account, the room, and the event.
It does not wait.
One router task owns the channel.
Provider calls take a permit from a semaphore of four, and each call times out at 10 seconds.
A hung peer then returns the permit.
One account, one registration, or one provider failing is logged and skipped.
It is never fatal to sync, and sync never awaits a provider.

Per registration and room, at most one send is in flight.
A newer eligible message replaces the pending alert before that send starts.
The in-memory pending map is disposable, same as the count-rise slot.

These bounds are the decision.
Tuning the numbers later is an implementation tweak, provided the sync loop still never awaits a provider and a hung call still cannot hold a permit without a timeout.

An unconfigured provider logs once at warn per process, then at debug, and skips.
The registrations stay.

### What the client does

#605, after #604 has landed:

- iOS enables the Push Notifications capability (`aps-environment`) and does not enable the `remote-notification` background mode.
  Alert pushes show the banner without a silent wake, and a silent wake is throttled and would not show one.
- iPadOS is that same build and the same token.
  A debug build sends `environment: sandbox`.
  TestFlight and the App Store send `production`.
- Android obtains an FCM registration token.
  The merged manifest still declares `POST_NOTIFICATIONS`.
  The push names channel `messages-v2` and does not mint a new channel id.
- The client does not keep a foreground service so the socket can survive Doze.
- macOS, Windows, Linux, and the browser do not request a remote token.
- On sign-out of the Axon session, and when message notifications are turned off, the client deletes the registration.
- A tap uses the listener #606 registered and opens the room.
- While the process is in the foreground, the client does not present the remote alert.
  The local notifier owns that toast.
- An event id the system already showed is written into the handled-id set #606 keeps, and that set survives a cold start, still capped at 2000.
  The reconnect slack must not post those ids again.
  Becoming active does not clear the system notification on its own.

### Web Push

Web Push is the same router with a third provider, later.
It needs an endpoint URL, a `p256dh` key, an auth secret, and VAPID config, which are not a device token.
`push_registrations` does not grow unused columns for them.
`push_provider_unsupported` is the seam.
A service worker is part of that later cut and is not added now.

## Consequences

- #604 is `crates/` only, plus the mechanical `clients/web/src/api/schema.d.ts` regeneration ADR 0099's exception already allows.
  It adds the migration, the routes, the config, the router, and the status field.
  OpenAPI stays compatible: both routes and the status field are new.
- #605 is `clients/web/`, including `src-tauri/`.
  It does not start until #604 has landed.
- `AGENTS.md` and the "What not to build" line in `docs/mvp/implementation.md` still say not to build a push router.
  They describe the code, which still has none.
  #604 corrects them in the change that adds the router, as ADR 0109 did for the management API.
- The threat model in `docs/mvp/tech-spec.md` is updated in this change, because the privacy choice is made here and the spec asked for that update in the push design.
- A self-hoster who configures neither provider keeps today's behavior.
  Clients may say that remote push is not configured, from `GET /v1/status`.
- Store listings should keep saying that a backgrounded phone receives nothing until #605 has shipped.
  ADR 0102's packaging scope is unchanged.
- `docs/client-parity.md` gains a row when #605 ships, not in this change.

## Alternatives rejected

**Wake the phone with a silent push and let the app post the local toast.**
The process is what suspension stops.
A silent push is throttled, and it shows nothing if the OS declines to run the app.
The alert itself is the notification.

**Skip delivery while the bearer's WebSocket is open.**
A suspended client can leave a socket the server still counts as live, for as long as the keepalive takes to fail.
That window is the outage this ADR is for.
The client suppresses the banner when it is actually in the foreground.

**Default to the message preview.**
The server has the plaintext, so this would be easy, and it is what the local toast already shows.
The local toast never leaves the device.
A preview in the push is readable by Apple or Google and by anyone who can see the lock screen.
The default stays `activity`.
A device opts into `sender_and_room` or `preview`.

**Put only an opaque id in the payload and open the room list on tap.**
#605's tap opens the room.
Doing that without a round trip means the payload carries the account, room, and event ids even at `activity`.
Those ids are the accepted metadata leak.
The alert text is what the setting controls.

**Encrypt device tokens with `store_key`.**
Right for Matrix access tokens (ADR 0008), because a database dump of those is account takeover.
A device token cannot be looked up under non-deterministic `pgp_sym_encrypt`, and possessing one does not grant a read.
Filesystem encryption remains the disk story, as it is for message content.

**One disclosure setting for the whole instance.**
A shared tablet and a personal phone are different devices.
The setting is stored on the registration the device writes.

**APNs for a quit Mac app, or a Windows push service.**
Declined above.
Desktop stays on the local notification the running process posts.

**Web Push in the first implementation.**
The registration shape would be wrong for it, and #602 leaves it as a later cut.

**An Axon-only mute list, or a mentions-only switch on the registration.**
The spec's `m.push_rules`, evaluated by the SDK into the counts ADR 0070 stores, already express per-room mute, mentions and keywords, sender rules, and the account default.
A parallel flag would drift from every other client on the account.
Per-device divergence from that shared ruleset is a later additive field, not part of v1.

**A durable retry queue for failed provider calls.**
Push is best-effort.
The next eligible message is the retry.
A queue is a second crash-recovery problem, and a 429 retry inside the sync loop is the failure mode the bounds exist to avoid.
