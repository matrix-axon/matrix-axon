# ADR 0104 — Mobile message gestures

**Status:** Accepted.
Server preference support is implemented here; the web client follows in a separate PR.

## Context

The web client exposes message actions through a tap-opened action bar.
On a phone, frequently used actions need a faster touch path without displacing the existing controls or keyboard access.

Axon supports several Matrix accounts for one human.
Gesture choices belong to that human, not to one Matrix account or one browser device.
ADR 0103's `instance_preferences` is therefore the synchronization boundary.

The mobile room view already uses a physical rightward swipe to close an open thread or return from the timeline to the room list.
That navigation must remain available from the whole timeline, including touches that begin on a message row.

## Decision

### Configurable gestures and actions

The initial configurable touch gestures are `double_tap`, `touch_and_hold`, and `swipe_left`.
A physical `swipe_right` remains reserved for mobile back navigation and is not a configurable message gesture.
A single tap continues to open the message action bar.

Each gesture maps to one of `reply`, `thread`, `react`, `edit`, or `delete`, or to JSON `null` for Off.
One action cannot be assigned to more than one gesture.
Delete opens the existing confirmation UI and never deletes immediately from a gesture.

The default preset is:

```text
double_tap     → react
touch_and_hold → thread
swipe_left     → reply
reaction emoji → 👍
```

`react` toggles the configured emoji through the same mutation used by reaction chips.
The configured value is one Unicode emoji, including one composed emoji sequence.

### Instance preference

Add the allowlisted `message_gestures` key to `GET` / `PUT /v1/preferences/{key}`.
Its complete v1 value is:

```json
{
  "schema_version": 1,
  "bindings": {
    "double_tap": "react",
    "touch_and_hold": "thread",
    "swipe_left": "reply"
  },
  "reaction_emoji": "👍"
}
```

The server rejects missing or unknown fields, unknown versions, unsupported gestures or actions, duplicate non-null actions, and a reaction value that is not one emoji.
There is no new table or migration.
ADR 0103's whole-value last-write-wins behavior, 64 KiB cap, `device_id` echo suppression, nil envelope `account_id`, and reconnect read remain unchanged.

An unset preference is the default preset.
A client does not write merely because GET returned 404.

### Gesture arbitration

Message gestures apply only to confirmed, unredacted, non-state message rows.
Pending and failed local echoes, redacted events, media tiles, and collapsed gallery groups are excluded initially.
Existing buttons, reaction chips, media controls, horizontally scrollable content, and multi-touch interactions keep their own behavior.

A leftward row swipe reuses the application's shared horizontal-swipe thresholds.
A rightward drag is never claimed by a row, so the room-level recognizer retains navigation.
Vertical movement cancels row recognition and remains timeline scrolling.

When double tap is configured, a touch single tap waits through the double-tap window before opening the action bar.
If double tap is Off, the action bar may open immediately.
Mouse behavior is unchanged.

When touch and hold is configured, the client suppresses native text selection and link preview on eligible message content.
When it is Off, those native behaviors return.

Tapping a confirmed event timestamp continues to copy its Matrix.to event link.
Touching and holding the timestamp copies the message body instead of invoking the row binding.
Turning touch and hold Off also disables timestamp hold-to-copy so native behavior returns consistently.

### Availability feedback

The action bar and gesture dispatcher share one eligibility implementation.
An ineligible row does not attach the gesture recognizer.
If a recognized gesture maps to a contextually unavailable action, such as editing another user's message, the client makes no mutation and shows a short row-local status message.

## Sequencing

The server change starts on PR 384's `feat/tags` branch while that PR is pending, then rebases onto `main` after PR 384 lands.
The web implementation follows as a separate client-silo commit and PR after the server contract is settled.
The web work includes the preference consumer, Settings UI, shared gesture recognizer, timestamp hold behavior, action dispatch, and mobile real-browser coverage.

Discoverability beyond the Settings surface is deferred to the application's broader discoverability work.
Media-tile and gallery-group gestures are also deferred until an interaction can be added without competing with media controls, lightbox opening, or gallery paging.

## Consequences

- Gesture choices follow every client connected to the same Axon instance, across all of its Matrix accounts.
- They do not follow the user to a separate Axon installation.
- Right-swipe navigation keeps one consistent meaning everywhere in the mobile timeline.
- Strict server validation keeps independently distributed clients from persisting configurations that no client can safely interpret.
- Whole-value last-write-wins means simultaneous edits on two devices do not merge individual bindings; reconnect GET is authoritative.
