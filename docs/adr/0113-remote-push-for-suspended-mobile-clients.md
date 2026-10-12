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

The router wakes a device for a newly stored eligible `m.room.message` when an observation of that room's `notification_count` or `highlight_count` rises, or when a fall in the same observation would otherwise hide that message.
The observations below are the whole rule.

The message is eligible when all of these hold:

- `event_type` is `m.room.message`.
- `content` is present.
  A still-undecryptable event does not wake anyone.
  Re-decryption does not re-emit a live frame, so a later back-fill does not send the push the UTD skipped.
  That drop is the normal case for an encrypted room whose key arrives after the event.
  #666 tracks the follow-up: an `activity` alert with no body, or a wake when re-decryption fills one.
  v1 does not do either.
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
Both `notification_count` and `highlight_count` live in `room_unread_counts`.
The unread path writes that row from room-info updates and from the five-minute resweep.
Event ingest does not write it.

The router is the only writer of the pair it compares against.
It does not read the unread task's memory.
A rise is either count strictly increasing.
One count rising still counts when the other falls.
In the cases below, `after` rose when either count is greater than its `before`.
`after` fell when neither rose and at least one count is lower.
`after` equals `before` when both counts are unchanged.

On startup the router seeds its observed pair from `room_unread_counts`, before it handles a live response.
That seed is not a rise.
A restart does not push unreads already stored, and it does not treat them as a `before` of zero.
The slot does not survive the restart.
The next notifying message after startup wakes the device.

For one sliding-sync response, #604 reads `room_unread_counts` before the unread path writes this response's count, and reads it again after that write.
The router receives that pair as `before` and `after`, together with the messages ingested from the same response.
The unread path publishing first cannot change `before`, because `before` was already the row.

A resweep, or a room-info update that is not part of a response the router is applying, is its own observation.
Its `before` is the pair the router last stored.
Its `after` is the pair the unread path just persisted.
It carries no message.
That observation can release a slot.
The release does not have to be a later sync response.

#604 reads `push_actions` on the live ingest path and passes them to the router.
The SDK sets them on a timeline event only when push context was supplied, and leaves them `None` otherwise.
The live path supplies that context.
Backfill and re-decryption keep passing `None`, and they still do not wake.
The router does not evaluate the ruleset a second time.
`Some` actions that include notify mark the message as notifying.
`Some` actions that do not include notify leave the slot unchanged, do not send, and do not consume an unpaired rise.
`None` means the actions are unknown, not that the rules declined to notify.
`None` may wake only when this same observation's count rose and the slot is empty, and only as that immediate send.
It never fills the slot, it does not claim an unpaired rise, and it does not send when the count fell.
A slotted notifying message is not replaced by it.
When none of those send conditions hold, the router drops that message.
The router logs each live `None` at warn with the account, the room, and the event id.
A once-per-process warning is not used, so a path that flaps stays visible.
A live path that never supplies context is a bug in #604, not the steady state.

The router keeps one slot per account and room.
The slot may hold only an eligible message whose actions are `Some` and include notify.
A newer message of that kind replaces the one already slotted.
A non-notifying eligible message does not fill the slot and does not clear it.
An event that is not an eligible message, including an invite, a call, a receipt, and a UTD, neither fills the slot, nor clears it, nor releases it.

The router decides, and only then stores `after`.
The first matching case wins.
A case that sends, releases, or re-bases also drops any unpaired rise.

- `after` rose and the observation has a notifying message.
  The router sends the latest one immediately and clears the slot for that account and room.
  One rise produces one wake.
  A message already slotted for that room is not sent as well.
- `after` rose, the slot is occupied, and the observation has no notifying message.
  The router releases the slot and sends the slotted message.
  This is the held-receipt case, including a resweep that publishes the rise on its own.
  A `None` message in that observation does not replace the slotted one.
- `after` rose, the slot is empty, and the observation has a `None` message.
  The router sends the latest such message immediately and does not fill the slot.
- `after` rose, the slot is empty, and there is no message to send.
  The router does not send.
  It remembers the rise as unpaired instead of consuming it.
  The unpaired rise expires, unsent, at the same five-minute interval as the slot.
- `after` equals `before`, an unpaired rise matches that count, and the observation has a notifying message.
  The router sends that message immediately and clears the slot.
  This is the rise that was published before the message arrived.
  A `None` message does not claim an unpaired rise, and a message does not otherwise inherit an earlier rise.
- `after` equals `before`, there is a notifying message, and no unpaired rise matches.
  The message fills the slot, or replaces the message already there, and the slot records `before`.
- `after` fell and the observation contains a notifying message.
  A read on another client can land in the same observation as the new message, so the net count is below `before` even though the message should wake.
  The router sends that message immediately, clears the slot, and stores `after`.
  It does not wait for a later count above the old `before`.
  A `None` message does not take this path.
- `after` fell and there is no notifying message.
  The room was read elsewhere.
  The router clears the slot without sending, drops any unpaired rise, and stores `after`.

Anything else leaves an occupied slot as it is.
A rise with an empty slot and no message sends nothing beyond the unpaired memory above.
The sent or released payload is that one message.
The slot is dropped, unsent, at the unread resweep interval (five minutes).
The slot is not durable.
A crash loses at most the wakes in flight, and the next notifying message wakes the device.

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

Two additive routes.
Both require a bearer `TokenVerifier` already accepts.
Any such bearer may register, including an OAuth access token, a token from `axon token issue`, and a token the management API minted for a second device.
Push does not widen what that bearer can already read through `/v1/`.
A narrower allow-list would reject those device credentials.
An unverified bearer is the existing `401`.
There is no separate rejection code.

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
  `apns` with a missing, empty, or other value is `400` with code `push_environment_invalid`.
  `fcm` with `environment` present is the same `400`.
  A re-PUT may change `environment`.
  The same session updates it, and a different session sets it while re-homing the row.
  A send to the wrong host can return `BadDeviceToken` and delete the row, so the client is allowed to correct the value before the next send.

`PUT` is an upsert on `(provider, token)`.
A repeat from the same session updates `disclosure`, `environment`, and the access token that last wrote the row, and returns the same row.
A repeat from a different session re-homes the row onto that session.
That is a sign-out followed by a sign-in on the same phone.
Two sessions that are both live and register the same device token are not that case.
The row belongs to the last writer.
The other session's `DELETE` is a `204` with no effect, and revoking the owner drops the row for both.
v1 accepts that.
#693 tracks a later design for two live owners of one device token.
The response is the row's `id`, `provider`, `disclosure`, and `environment`.
It does not echo the token.

`DELETE` removes the row when the caller's session owns that `id`.
An id that is absent, or that belongs to another session, also answers `204`, so the route is idempotent and does not reveal other devices.
A client that has forgotten the id calls `PUT` again and deletes the returned id.

Ownership is a credential session, not the access token's `tokens.id`.
An OAuth access token expires, and refresh mints a new row (ADR 0054).
Keying the registration on that id would make `DELETE` after refresh a silent no-op, and the server would keep pushing.
Treating expiry as revocation would wipe the registration on every refresh.

#604 adds `session_id` to `tokens` and to `oauth_refresh_tokens`.
OAuth sign-in mints one session id and stores it on the first access token and the first refresh token.
Every refresh copies that same id onto the new access token and the new refresh token.
A non-expiring credential, whether issued by the CLI or by the management API, uses its own `tokens.id` as its session id.
Those credentials do not rotate.
The session id is not `oauth_identity_id` and not `client_id`.
Both of those are shared by every device of the same person.

Rows that already exist have no shared id.
An access token and its refresh token are not linked (ADR 0054), so the migration cannot pair them.
It sets `tokens.session_id` to that row's own `id`, and `oauth_refresh_tokens.session_id` to that row's own `id`.
Each legacy credential is its own session.
The next refresh of a legacy refresh token copies that refresh token's session id onto the new access token and the new refresh token.
The pre-migration access token keeps its own session id until it expires.
`NULL` is not a session.
The migration does not leave `session_id` null, a registration insert requires one, and the reaper skips a null rather than grouping every null row into one session.

A `PUT` with an expiring access token whose session has no refresh token is `400` with code `push_session_refresh_required`.
The row is not written.
That token is a legacy access token: its session id is its own row id, and the device's refresh token is a different session.
Registering it would make the send-time check delete the row when the access token expires, even though the refresh token is still valid, and a suspended app cannot re-`PUT`.
#605 refreshes once before the first `PUT`, and refreshes again when it receives that code, then repeats the `PUT` with the new access token.
That token carries the refresh token's session id, so expiry of the access token does not reap the registration.
A non-expiring credential has no refresh token and is not rejected by this check.
The liveness check does not treat another device's refresh token as belonging to this session.
`oauth_identity_id` and `client_id` are shared by every device of the same person, so they are the wrong key for that.

The row lives in `push_registrations`:

- `id` UUID primary key.
- `session_id` UUID, the credential session that owns the row.
- `token_id` UUID, the access token that last wrote the row.
  Authorization does not use it.
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
Removing a Matrix account drops queued wakes and the count-rise slot for that `account_id`.
The device registration stays, because it still covers every account that remains.
A per-account opt-out is a later additive field and is not in v1.
The client deletes the registration when message notifications are turned off, and when the Axon session ends.

A failed `DELETE` leaves the local setting off and retries.
The client does not turn the setting back on to match a row the server still has.

The client also re-PUTs after a successful refresh, so `token_id` and `disclosure` stay current.
A missed re-PUT does not make `DELETE` a no-op, because the refreshed access token carries the same session id.

Revoking a session deletes its registrations in the same transaction.
For a non-expiring credential, that is the transaction that sets `revoked_at`.
For an OAuth session, that is the transaction that revokes the refresh token and the access tokens of that session.
Boot deletes a registration whose session has no unrevoked unexpired access token and no unrevoked unexpired refresh token.
An expired access token with a live refresh token does not reap the row.
The same check runs before each provider send.
A session that fails it drops that wake, deletes the registration, and does not call the provider.
A process that stays up for weeks therefore stops pushing once the last credential for that session has expired, without waiting for the next boot.
Neither `session_id` nor `token_id` has a foreign key.
There is no session table for `session_id` to reference.
A cascade from `token_id` to `tokens.id` would delete a live registration when that one access-token row is removed, which is the loss the session id exists to avoid.
Revocation itself is not a delete of the registration's authorizing row, which is why the revoke transaction, the send-time check, and the boot pass all exist.

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

An APNs `410` with reason `Unregistered` deletes that registration only when the response `timestamp` is strictly later than the row's `updated_at`.
That timestamp is milliseconds since the epoch, the last time APNs confirmed the token was no longer valid for the topic, and the body includes it only on a 410.
A missing timestamp does not delete.
A timestamp earlier than or equal to `updated_at` does not delete.
A device that re-registers the same token bumps `updated_at`, so a 410 from a send already in flight cannot remove the new row.
An APNs `400` with reason `BadDeviceToken` deletes that registration.
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
The send-time liveness check runs in that router task, on the same permit and under the same 10-second timeout as the provider call.
It does not run on the sync loop.
A failed check, whether the query errors or the timeout fires, is fail-open: the router sends, logs, and does not delete the registration.
The next send checks again.
Only a successful answer that the session has no live credential drops the wake and deletes the row.
A transient database error must not wipe a registration, just as a failed name read still sends.
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
- Before the first `PUT`, the client refreshes an OAuth session once, and it refreshes again when the server answers `push_session_refresh_required`.
  The `PUT` uses the access token from that refresh.
  A non-expiring credential is registered as it is.
- A tap uses the listener #606 registered and opens the room.
- While the process is in the foreground, the client does not present the remote alert.
  The local notifier owns that toast.
- No app code runs when an alert is delivered to a suspended process.
  v1 does not enable the `remote-notification` background mode, and it does not add a notification service extension, in order to record that delivery.
- On becoming active, before the reconnect slack posts, the client reads the notifications the system is still showing.
  Apple uses `UNUserNotificationCenter.getDeliveredNotifications`.
  Android uses the active notifications for this package, and the event id is the `event_id` extra from the data payload.
  Each of those ids is written into the handled-id set #606 keeps.
  That set survives a cold start and stays capped at 2000.
  The reconnect slack must not post those ids again.
- An alert the user dismissed while the process was suspended is not in that set.
  No app code ran to record the swipe, and v1 does not add a background mode or a notification service extension to observe it.
  The local notifier may toast that event again inside the reconnect slack.
  v1 accepts this.
- An alert that coalesce replaced is no longer delivered, so its event id was never shown.
  The local notifier may still toast it when it falls inside the reconnect slack.
  That is the first time the user sees it.
- Becoming active does not clear the system notification on its own.

### Web Push

Web Push is the same router with a third provider, later.
It needs an endpoint URL, a `p256dh` key, an auth secret, and VAPID config, which are not a device token.
`push_registrations` does not grow unused columns for them.
`push_provider_unsupported` is the seam.
A service worker is part of that later cut and is not added now.

## Consequences

- #604 is `crates/` only, plus the mechanical `clients/web/src/api/schema.d.ts` regeneration ADR 0099's exception already allows.
  It adds the migration, including `session_id` on the credential tables, the routes, the config, the router, and the status field.
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

**Release a slotted message only when a later sync response carries a higher count.**
The held-receipt rise is published by the unread path or the resweep, which may not be a response at all.
Waiting only for a later response drops that wake at five minutes.
The router reads `room_unread_counts` itself, and a resweep observation can release the slot.

**Consume a rise that arrives with no message.**
The resweep can publish that rise before ingest stores the message.
Forgetting it leaves the message slotted at the new count, with no later rise to release it.
The router remembers an unpaired rise until a notifying message claims it or the five-minute interval expires it.

**Keep a legacy registration alive because some other refresh token for the same person is unexpired.**
`oauth_identity_id` and `client_id` are shared by every device.
That would keep pushing a session whose own refresh token is gone.
The server rejects the `PUT` until the client refreshes, so the registration is born on the refresh token's session.

**Clear the slot when a non-notifying message arrives.**
The slot never holds a message the rules did not mark as notifying, so the clear does not prevent a suppressed push.
It does drop a notifying message whose count rise is still held.

**Cascade `push_registrations.token_id` when `tokens` loses a row.**
That access token is the last writer, not the owner.
Removing that one row would delete a registration the session still owns.
