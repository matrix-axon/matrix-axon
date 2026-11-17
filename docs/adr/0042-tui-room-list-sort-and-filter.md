# ADR 0042 — TUI room list sort & filter modes

## Context

The TUI room list offers a single fixed ordering — pinned rooms first (ADR 0038),
then the rest by recent activity (`Reverse(last_activity_ts)`) — and one ad-hoc
filter: an unread-only toggle (`alt-u`, ADR 0037). Users with many rooms want to
slice the list quickly: show only direct messages, only group rooms, only unread,
only favorites, or only rooms whose name matches a typed string; and to sort
either by recent activity or alphabetically, in either direction.

Two relevant capabilities already exist and are reused rather than rebuilt:

- **Pinning** (ADR 0038): `App.pinned_rooms: Vec<RoomKey>`, `sort_rooms_by_pin()`,
  and the pinned/unpinned separator. Pinned rooms must remain on top in every
  mode, in their user-defined pin order.
- **Unread filter** (ADR 0037): `App.unread_filter: bool` applied in
  `visible_room_indices()`.

The one piece of missing data is **DM vs. group classification**. `RoomDto`
carries no `is_direct` field; the rooms table has no such column. The signal does
exist server-side — `m.direct` global account data is persisted to the
`account_data` table — but exposing it to the client is an API-silo change being
designed separately in **ADR 0043 (room metadata exposure)** / PR #174, which
proposes a server-derived curated `is_direct` field on the room-list DTO.

PR #155 already ships an interim heuristic: a room with no `name` and no
`canonical_alias` is treated as an unnamed room / DM (used by
`request_unnamed_room_titles` and `dm_title_from_members`).

## Decision

Implement sort & filter modes as a pure TUI change (one silo), reusing the
existing pin, unread, search, and config-save machinery.

### Filter modes

Replace the standalone `unread_filter: bool` with a single source of truth:

```rust
enum RoomFilter { All, Dms, Groups, Unread, Favorites, Name(String) }
```

Applied in `visible_room_indices()` (`app.rs`), with the account filter remaining
the always-applied outer filter:

- `All` — no room-level filtering.
- `Unread` — unread count > 0 (existing logic).
- `Dms` / `Groups` — `is_likely_dm(room)` and its complement.
- `Favorites` — `is_room_pinned(room)`.
- `Name(q)` — the existing name/alias/topic/room_id predicate plus the same
  rendered room-list title shown to the user, so member-derived DM titles match.

The selected room is always kept visible (existing behavior); on a filter change
the selection is clamped to the first visible row.

**Interim DM heuristic.** `is_likely_dm()` returns true when both `name` and
`canonical_alias` are empty/blank — the PR #155 heuristic, factored into one
helper in `app/rooms.rs` and shared with `request_unnamed_room_titles`. This is
imperfect (a named two-person room reads as a group; an unnamed small group reads
as a DM). It is explicitly interim: once ADR 0043 / PR #174 lands a curated
server `is_direct`, the TUI switches `is_likely_dm()` to consume it. This ADR
records that follow-up.

### Sort modes

```rust
enum RoomSort { RecentActivity, OldestActivity, AlphaAsc, AlphaDesc }
```

`sort_rooms_by_pin()` is generalized to take the active `RoomSort`. The **pinned
section keeps its pin-position order** (ADR 0038, user-defined) in all modes; the
chosen comparator is applied only to the unpinned tail:

- `RecentActivity` — `Reverse(last_activity_ts)` (current default).
- `OldestActivity` — ascending `last_activity_ts`.
- `AlphaAsc` / `AlphaDesc` — case-insensitive compare on the rendered
  room-list title, so unnamed DMs sort by their member-derived display names
  once known instead of by opaque room IDs.

Existing call sites (`apply_room_refresh`, `resort_rooms`) pass `self.room_sort`;
changing the sort mode re-sorts in place.

### Keyboard shortcuts & commands

New configurable bindings (`config.rs` `Shortcuts`, handled globally next to
`toggle_unread_filter` in `keymap.rs` so they work from any focus; alt-modified
so unmodified characters still reach the compose box):

| Field | Default | Action |
|---|---|---|
| `room_filter_cycle` | `alt-f` | cycle All→DMs→Groups→Unread→Favorites (skips Name) |
| `room_sort_cycle` | `alt-s` | cycle Recent→Oldest→A–Z→Z–A |
| `room_filter_unread` | `alt-u` | set Unread (repurposed from the toggle) |
| `room_filter_dms` | `alt-d` | set DMs |
| `room_filter_groups` | `alt-g` | set Groups |
| `room_filter_favorites` | `alt-v` | set Favorites |
| `room_filter_all` | `alt-0` | clear to All |
| `room_filter_by_name` | `alt-/` | enter name-filter text input |
| `room_sort_recent` | `alt-1` | Recent; repeat toggles ↔ Oldest |
| `room_sort_alpha` | `alt-2` | A–Z; repeat toggles ↔ Z–A |

The name-filter input reuses the existing per-keystroke `Mode::Search` plumbing
via a new `SearchKind::RoomNameFilter`: each keystroke updates
`RoomFilter::Name(query)` live (incremental hide); Enter keeps it, Esc reverts to
the prior filter. This is distinct from the existing jump-to-match `/` search
(`commit_room_search`), which is unchanged.

Equivalent slash commands (`command.rs`) mirror `/pin` for discoverability and
scripting: `/filter [all|dms|groups|unread|fav|<text>]` and
`/sort [recent|oldest|az|za]`.

`popup_shortcuts_lines` (`ui.rs`) lists the new bindings in the room-list
section, and the room-list block title shows the active filter and sort.

### Persistence

Following ADR 0038, the chosen sort mode and filter **category** persist in
`[display]` (`room_sort`, `room_filter`), saved via the existing
`TuiConfig::save_display()` `toml_edit` path and restored in `App::new()`.
Unknown values fall back to defaults (`all` / recent activity). `Name(_)` is
session-only — it persists as `all`, since restoring an arbitrary stale query
string is surprising.

#### Web amendment: persisted room-list preferences

The web client intentionally differs from the TUI persistence decision above.
At the user's explicit request, sort, category, Favs, name query, and account selection persist in browser-local settings across sessions.
The restored name query remains visible in the search field with a clear control; it overrides the category until cleared.
This deliberately preserves a narrowed room list even when a deep link or notification opens a room outside the query, so unread rooms outside that query remain hidden until the query is cleared.
Favs is an independent favorites-first toggle, not a favorites-only category, and is outside the category shortcut cycle.
An unknown or unavailable saved account uses an All-accounts fallback without overwriting the saved selection, so temporary account deactivation does not lose the preference.
Name-query saves are debounced and flushed on blur or page hide; saves merge only fields changed by this tab into the latest local settings envelope to preserve unrelated changes from another tab.

### Not chosen

- **Block DM/Group on the backend.** Deferring those two modes until ADR 0043
  ships would withhold most of a useful, self-contained TUI improvement. The
  interim heuristic is non-destructive and swaps out behind one helper.
- **Mentions-only filter.** The TUI has no per-room mention/highlight count yet;
  deferred until that data exists.

## Consequences

- The room list can be filtered to DMs, groups, unread, favorites, or a typed
  name substring, and sorted by activity or name in either direction; pinned
  rooms stay on top in pin order throughout.
- `unread_filter: bool` is removed in favor of `RoomFilter::Unread`; `alt-u`
  keeps its meaning (now "set Unread filter").
- The `[display]` section gains `room_sort` and `room_filter` keys; older TUI
  builds ignore them gracefully.
- DM/Group accuracy is limited by the interim heuristic until ADR 0043 / PR #174
  provides a curated `is_direct`; the switch is localized to `is_likely_dm()`.
- No backend changes.
