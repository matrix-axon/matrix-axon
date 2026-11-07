# ADR 0130 — TUI invite inbox

**Status:** Proposed; implemented for the TUI invite inbox, part of [issue 222](https://github.com/matrix-axon/matrix-axon/issues/222).

## Context

[ADR 0091](0091-invited-room-visibility.md) stores pending invites in `room_invites` and publishes them through `GET /v1/invites` plus `invite.added` and `invite.removed`.
Accept is the existing room join.
Reject is the existing room leave.
The web client already shows an Invites row and an `/invites` inbox.
The TUI had no consumer, so a pending invite was invisible there.
[ADR 0040](0040-known-contact-dm-auto-join.md) may still auto-join a known-contact DM, which can flash one invite and then remove it.
This ADR is the TUI consumer.
It does not change the server.

## Decision

### Joined rooms stay the selectable list

The room list's selectable rows remain joined rooms.
A pending invite is not a row in that list, and it is not a keyboard stop the way the web Invites row is.
When the filtered invite count is above zero, the rooms pane title gains ` · N invites` after the title it already had, including a bootstrap or search title and the unsaved-order suffix.
One invite uses the singular.
Zero invites leaves the title unchanged.

### `/pending` is the inbox

`/pending` opens a popup whose title names the configured accept, reject, and close keys.
The name leaves `/invite` as its own Tab completion for inviting someone into the current room.
With the defaults that title is "Invites  (Enter accept, R reject, Esc close)".
The letter in a shortcut label is shown in uppercase, which is how the shortcuts popup already prints `r`.
Up and Down move a selection stored as the invite's account and room, so a refresh does not jump to another row.
Page Up and Page Down move by eight rows, and the configured message page keys do the same.
Enter accepts the invite that is still selected.
Reject uses the `reject_invite` shortcut.
The default is `r`, with no modifiers, so Shift-R and Ctrl-R do not reject unless the user binds them.
When submit and `reject_invite` are the same key, accept wins.
The popup title omits reject when that key is also submit, or when it is a shortcut handled before the popup.
A page binding is checked after reject, so the title still names reject and paging is what that key no longer does.
Reject asks `Reject {title}? [y/N]` and stays in the inbox.
The inbox popup stays up under that question.
The popup title is the question, shortened so `[y/N]` remains, and each frame puts the question back on the status line.
`y` sends the leave for the invite that was selected when the question was asked.
If that invite has already left the list, `y` does not send and says it is no longer pending.
`n` and Esc cancel, return to the popup, and leave the composer as it was.
A reject that is still held, waiting on its first check, shows the pending status and does not ask.
The question does not start the action, so it does not mark a room action busy and it does not clear a join watch.
Focus-cycle leaves the question the same way it leaves a room-action confirmation, and it does not reject.
A filter that hides the selected row moves the marker onto an invite that is still visible.
Enter and reject on that same keystroke do not act on the newly chosen row.
The next frame draws the marker on the row that is still visible.
Esc closes the popup through the existing popup path.
An empty account filter says "No invites."
Invites that exist and fail only the room filter say "No invites match this filter."
Opening the popup asks for a fresh list.
The same list is also requested when accounts land at startup, after a login, logout, recover, backup enable, or delete refreshes the account list, on `/refresh` and the refresh shortcut, and on a reconnect that `note_connected_frame` accepts.
A target-resolution read that has not changed the active set yet does not request it.
That startup read is spawned beside the room fetch.
It does not add a stage in front of device state, and applying it does not advance bootstrap.

### The list is cross-account, then filtered locally

`GET /v1/invites` is always sent with no `account_id`.
The body is capped at 1 MiB.
Each invite is keyed by `(account_id, room_id)`.
The account on a live frame is the WebSocket envelope's account, because `invite.removed` carries only `room_id`.
An `invite.added` whose payload account differs from the envelope is a protocol error.
The visible set uses the room list's account rules: hide inactive accounts, and honor both the launch account and the accounts-panel selection.
The room filter then applies.
All and Unread keep the remaining invites, because a pending invite is unseen attention.
DMs keep `is_direct`.
Groups keep the rest.
Favorites keep none, so that filter shows no invite suffix.
A name filter is a case-insensitive substring of the title, alias, room id, and the inviter's user id and display name.
The display title is a nonblank name, then the canonical alias, then the room id.
A row names the account localpart only when the room list would.

### A 404 is an older server

A 404 on the list, including an empty body, means the server has no invite route.
The inbox shows empty and the status line stays as it was.
That response does not clear suppression and does not settle a pending reject.
Any other list error is reported as an invite refresh failure, unless a prompt owns the status line, and the last good list stays.

### Suppression stops a stale list from resurrecting a row

Accept and reject that return 2xx drop the row locally and suppress that key until a later GET omits it.
Both also request that list.
The accept does not clear suppression itself.
The refresh is what lets a later re-invite show when its `invite.added` frame was missed.
`invite.added` clears that suppression, so a re-invite shows, and it does not by itself end a pending reject.
`invite.removed` drops the row, ends a pending reject, and does not suppress.
Frames that arrive while a GET is in flight are merged into that response.
A failed read leaves those frames in place, and the retry merges them, so the retry's snapshot cannot put a removed invite back or drop an add from that failed read.
Live adds are a newest-first list.
Ones the server also returned stay with the server rows.
Ones the server omitted stay in front.
A response that started before a local drop cannot put the row back.
A known-contact add followed by a removal leaves the inbox empty.
That flash is allowed.

### A reject 504 uses the leave schedule and the invite list

A reject that returns 504 keeps the row and does not suppress it.
It starts a pending hold on its own map, separate from pending membership, using the leave delays of 30, 90, 210, and 450 seconds and the same final grace.
A repeat reject is held until the first check has run.
After that, a deliberate retry is sent.
A retry that fails outright ends the hold.
A second 504 replaces the schedule.
Settlement reads `GET /v1/invites`.
A snapshot that started before the 504 says nothing.
The list is unfiltered, so a successful read covers every account that is still active.
A logged-out account is skipped: its rows are absent because the account is gone, and that absence does not end the hold or mark the invite seen.
The poll does not advance that hold or request a list for it until the account is active again.
A later list that still contains the key marks it seen.
A later list that omits the key, including an empty successful list, ends the hold for an account that is still active.
A failed read and a 404 say nothing about the hold.
When the window ends and the invite was still listed, the status line asks the user to reject it again.
While the hold is not yet retryable, the status line says the homeserver has not finished and that another reject can wait.

### Accept follows the room once the join is known

Accept calls join for the invite's account and room.
A 2xx suppresses the invite, requests a fresh invite list, closes the popup, refreshes the room list, and opens the resolved room when that list contains it.
The resolved id falls back to the invite's own room id.
A 504 does not suppress the invite.
It closes the popup, requests an invite-list read immediately, and two more on the same delays as a join that was not listed yet.
It opens the room when the room list already contains the invite's room id, and otherwise watches the room list the way `/join` does.
It does not put `/join` back into the composer.
Those extra invite reads belong to the accept.
`/join` does not issue them.
The popup closes before that room open, so the open is not deferred behind the popup.
Any other accept error keeps the popup and the row and reports the failure.
Accept and reject share the single in-flight room action.
A busy action stays in the popup and does not send.
A held repeat reject returns before that busy check and does not clear a join watch.

## Consequences

- The TUI and the web both consume ADR 0091.
  The TUI keeps the room list to joined rooms and uses a count plus a popup.
- An older server degrades to an empty inbox with no error.
- A reject the homeserver has not answered yet stays visible and settles from the invite list on the leave schedule.
- An accept the homeserver has not answered yet uses the room-list watch, re-reads the invite list, and leaves the composer alone.
- A logged-out account's pending reject is not settled by a list that omits that account.
- Known-contact DM invites may still appear for one update and then disappear.
- Selectable room navigation is unchanged.
- Reject is the `reject_invite` shortcut, `r` by default, and it asks before it sends the leave.
