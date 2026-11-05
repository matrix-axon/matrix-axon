# ADR 0129 — Badge and read clearing for suspended phones

**Status:** Proposed; tracked in [issue 958](https://github.com/matrix-axon/matrix-axon/issues/958).
Amends [ADR 0113](0113-remote-push-for-suspended-mobile-clients.md).

## Context

ADR 0113 wakes a suspended phone with a visible alert when a room's count rises.
It sends nothing when a count falls, and its payload "carries no badge count".
The icon badge is whatever the running app last painted from `RoomsStore.unreadTotal` (ADR 0080, #607).

So a room read somewhere else leaves a suspended phone out of date:

- On iOS and iPadOS the icon keeps the number from before suspension.
  It does not rise either, because the alert carries no count.
- On Android the launcher badge comes from the app's active notifications.
  The shell has no badge API (`setIconBadge` is null there).
  A notification for a room that has since been read stays in the shade and keeps the launcher dot lit.

The read can come from any client.
A read receipt reaches the homeserver, sync lowers `room_unread_counts` (ADR 0070), and the unread watcher hands the push router a count observation.
The router already sees the fall.
In ADR 0113 the fall only clears the room's slot.

Desktop is not part of this.
A running desktop app hears `unread_counts.changed` on the socket and refreshes the room list after a reconnect, so its badge already follows reads.
Removing delivered notifications on desktop, and from iOS Notification Center, is [issue 959](https://github.com/matrix-axon/matrix-axon/issues/959).

## Decision

The server keeps a suspended phone's unread state current with two new sends.
No send runs app code on iOS.
ADR 0113's case list is unchanged: an alert is queued in exactly the cases it was before.
One alert that was queued may now go unsent: on an Android device, a room-read message replaces a waiting alert for the same room (see Router and scheduling).
The only change to the alert itself is the `aps.badge` field described under iOS below.

### The total

The badge number is the instance's unread total: the sum of `notification_count` over the rooms `GET /v1/rooms` lists.
That is the sum `RoomsStore.unreadTotal` keeps, so a phone that wakes and repaints from the room list shows the number the push showed.
The store computes it with the same joins and visibility predicates as `list_rooms`: active accounts only, and not `hidden_left` or `hidden_tombstoned`.
The two queries share that SQL so they cannot drift.

The total is read when a send starts, not when the router decides.
A burst of reads that coalesces into one send carries the newest total.
A failed or slow read (the two-second bound the liveness check uses) skips the badge, and the failure is logged.
For an alert, only the badge field is skipped and the alert still goes out.
The devices that share one alert share one successful read.
A failed read is not shared: the next device tries again.

One registration covers every Matrix account on the instance (ADR 0113), so one total covers the device.

### iOS: the badge rides every push

A registration has a new `badge` flag.
When it is true and the provider is APNs:

- Every alert carries `aps.badge` set to the total.
  The alert's other fields, headers, and collapse id are unchanged.
- When a seeded room's `notification_count` falls, the router queues a badge-only push for that device.
  Its body is `{"aps":{"badge":N}}`, with `N` read at send time.
  `apns-push-type` is `alert`, as Apple requires for a badge.
  `apns-priority` is `5`: the number only matters once someone looks at the home screen, so the device's power policy may batch it.
  There is no `apns-collapse-id` and no `content-available`.
  No app code runs, so this still needs no background mode.
- An observation that falls and also sends an alert (ADR 0113's "fell, still above zero, notifying message" case) does not queue a second, badge-only push.
  The alert already carries the total.
- If a badge-enabled device's alert cannot go out with its total, the outcome depends on why:
  - An alert too large for the payload limit, even with the generic text, is sent as a badge-only push instead, so the total still lands.
  - A newer alert for the same room that replaces a waiting one carries a newer total.
  - A registration the liveness check rejects is deleted, so its badge no longer matters.
  - A failed or slow total read sends the alert without `aps.badge`, and no badge-only push follows: it would need the same read.
    The icon then stays one step behind until the next push, or until the app runs and repaints.
    v1 accepts this.
- A zero total is sent as `0`, which clears the icon.

The badge-only push goes out on every fall, not only when the total reaches zero.
A partial read on another device (5 rooms unread, 4 read) has to take the phone's badge from 5 to 1.

That has a cost Adam accepted when this was designed.
For an offline device, APNs keeps only the newest notification per app.
If a phone is offline (not just suspended) with an alert stored for room A, a partial read of room B elsewhere replaces that stored alert with a badge-only push.
The phone then never shows A's banner.
Its badge still counts A, and the room list still shows A as unread.
ADR 0113 already accepts the same loss between two alerts.

Ordering between pushes is best-effort.
APNs does not promise to deliver two pushes in the order they were sent.
A badge-only push that overtakes a later alert can leave the badge one alert behind until the next push, or until the app runs and repaints.
v1 accepts this.

A device with `badge` false gets exactly ADR 0113's payloads.
While the app is in the foreground it keeps painting the badge itself, as it does now.

### Android: cancel a read room's notification

When a seeded room's `notification_count` falls to zero, the router queues a room-read message for every FCM registration.
`badge` does not gate this.
Withdrawing the notification for a room that has been read is not a badge preference, and Android has no separate badge number to opt out of.

The message is data only.
It has no `notification` block, so the system shows nothing:

```json
{
  "message": {
    "token": "<device token>",
    "data": {
      "axon_kind": "room_read",
      "account_id": "<uuid>",
      "room_id": "!room:example",
      "tag": "<collapse value>"
    },
    "android": { "priority": "NORMAL" }
  }
}
```

`tag` is ADR 0113's collapse value for the room, the same tag the alert and the local toast already use.
The client cancels the notification with that tag and id `0`.

The priority is `NORMAL` on purpose.
FCM may demote an app's high-priority messages when they do not lead to a visible notification, and the alerts need to stay high priority.
A normal-priority message can wait for a Doze maintenance window, so a clear can arrive late.
There is no `collapse_key`.
FCM stores at most four collapse keys per device, and one per room would drop clears for most rooms.

A late clear must not cancel a newer alert for the same room.
When the client posts an alert, it stores that alert's FCM `sentTime` (`RemoteMessage.getSentTime()`) in the notification's extras.
A room-read message cancels the notification only when the stored time is earlier than the room-read's own `sentTime`.
Both times come from FCM's servers, so the phone's clock is never compared, and an alert sent after the clear survives it.
The notification's `postTime` is not used: it is the phone's clock.
A notification with no stored time, posted by an older build or by the running app, is cancelled.
The running app is in the foreground or recently was, so a normal-priority clear is not held for Doze, and the app repaints from the room list anyway.

A partial read (the count falls but stays above zero) sends no room-read message.
The room still has unread messages, and its one collapsed notification is still accurate.

An older client already drops this message.
`AxonFirebaseMessagingService.handleIntent` passes a message without `event_id` to the SDK, the SDK calls `onMessageReceived` for a data-only message, and that method does nothing.
So the server half can ship before the client half.

### Router and scheduling

Both new sends use ADR 0113's machinery: the same snapshot of registrations, the liveness check before each send, the semaphore of four, the ten-second provider timeout, and the provider verdicts that delete a dead device token.
The sync loop still never awaits a provider.
The router task still does not await Postgres: the total is read inside the spawned send.

- A room-read message uses the same in-flight key as that room's alerts (account, registration, room).
  It cannot run at the same time as an alert for that room.
  A newer alert or room-read message replaces one that is still waiting, so a waiting alert for a room that has since been read is dropped instead of sent.
- A badge-only push has one key per registration.
  At most one runs and at most one waits, so a burst of reads (marking every room read) produces at most two badge-only pushes per device.
  A fall that arrives while a badge-only push is running always fills the waiting slot, even if the running push has already read its total.
  The waiting push reads the total again when it starts, so the last push a burst produces carries the newest total.
- A room-read message is account-scoped like an alert, so removing the account or dropping the room drops it while it waits.
  A badge-only push is not tied to an account.
- The first observation of a room after startup seeds the room and is not a fall, as in ADR 0113.
  A read that lands before a room's first observation does not move the phone's badge until the next push.

Leaving a room, or removing an account, lowers the total without a count observation.
That sends nothing.
The next push or the next foreground repaint corrects the badge.

### Registration

`PUT /v1/push/registrations` gains an optional boolean `badge`.
An omitted `badge` is stored as false, so a client built before this change keeps ADR 0113's payloads, and a device opts in explicitly.
The upsert writes it like `disclosure`: a repeat `PUT` that omits it sets it back to false.
So every client `PUT` must send `badge`, including a re-registration after a token refresh; otherwise the device silently stops getting badges.
The response echoes `badge`.

The field is accepted for both providers.
Only APNs reads it now.

The column is `push_registrations.badge BOOLEAN NOT NULL DEFAULT false`, added by a new migration.
The change is additive, so the OpenAPI compatibility check passes without an exception.

The client sends its existing `appBadgeEnabled` setting as `badge` (ADR 0080), and `PUT`s again when the user changes that setting.
There is no second badge setting.

### Logging

A badge-only push logs at debug on success with the provider, the registration id, the total, and the HTTP status.
A room-read message logs the account and the room, as an alert does.
A failure logs as ADR 0113 describes.
Neither logs the device token, the authorization header, or the body.
The total is an aggregate count and is not secret.

## Consequences

- The server change is `crates/` only, plus ADR 0099's mechanical regeneration of `clients/web/src/api/schema.d.ts`.
  It adds the migration, the `badge` field, the total query, the two payloads, and the router's fall handling.
- The client change waits for the server change, as ADR 0113 required for its own client half.
  It is `clients/web/`, including `src-tauri/`:
  - Every `PUT` sends `badge` from `appBadgeEnabled`, including token-refresh re-registrations, and changing that setting re-registers.
    A test pins that no registration path omits the field.
  - The Android service stores each alert's FCM `sentTime` in the notification's extras and handles `axon_kind: room_read`.
    It cancels the room's tag when the stored time is earlier than the room-read's `sentTime`, on the background path and on the foreground path.
- `docs/client-parity.md` gains a row when the client half ships.
- iOS Notification Center still keeps a banner for a room that has been read.
  Withdrawing it needs app code on a suspended phone, which this ADR does not add.
  Issue 959 tracks it.
- Each read now costs provider calls: one badge-only push per badge-enabled iOS device (coalesced), and one room-read message per Android device when a room reaches zero.
  Marking many rooms read on desktop sends one room-read message per room to each Android device.
  That is far inside FCM's per-project quota, and the semaphore of four still bounds concurrency.

## Alternatives rejected

**Badge-only push only when the total reaches zero.**
It never replaces a stored alert for a room that is still unread.
But after a partial read the badge stays high until the next alert or a full read, and partial reads are the common case.
Adam chose "every fall".

**Debounce falls for about thirty seconds before sending.**
The scheduler already coalesces a burst into at most two pushes per device.
A fixed delay makes every clear slower, and the burst costs only one extra push.

**A silent `content-available` push, so the iOS app recomputes the badge and withdraws banners.**
ADR 0113 rejects silent wakes: iOS throttles them, and may not run the app at all.
The badge field needs no app code.
Withdrawing banners is a separate decision (issue 959).

**High-priority room-read messages on Android.**
They would arrive sooner in Doze.
FCM may demote high-priority messages that show nothing, and that demotion would also slow the alerts.

**One collapse key for every room-read message.**
A device that was offline would receive only the newest clear, and every other read room would stay in the shade.

**Badge on by default when `badge` is omitted.**
A client built before this change would get badge numbers on alerts.
Its badge setting would not control them, and it could not turn them off without an update.

**Let the client compute the badge from a pushed per-room delta.**
The client is suspended, and no app code runs to apply a delta.
Only a number that APNs applies on its own works without a background mode.
