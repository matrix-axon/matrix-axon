/**
 * What Room Information shows for each server state of the typed room-detail
 * reads (ADR 0133 decision 3). Every row goes through here, so no row invents
 * its own wording for "unknown", "redacted" or "stale".
 *
 * Rows 1–7 and 11 of the metadata table use the TUI's phrases for the same
 * states (`clients/tui/src/app/room_info.rs`: `no_content_phrase`,
 * `redaction_note`, `scalar`), capitalized for the web panel.
 */
import type { components } from '../api/schema'
import { relativeTime } from '../relative-time'

type Schemas = components['schemas']
export type RoomMetadataDto = Schemas['RoomMetadataDto']
export type RoomHomeserverDetailsDto = Schemas['RoomHomeserverDetailsDto']
export type RoomInfoDto = Schemas['RoomInfoDto']
type DetailRefreshDto = Schemas['DetailRefreshDto']
type LocalAliasesDetailDto = Schemas['LocalAliasesDetailDto']
type DirectoryVisibilityDetailDto = Schemas['DirectoryVisibilityDetailDto']

/**
 * One displayed detail. `value` is real room data, and the only thing worth
 * copying; when it is `null`, `label` is the state shown in its place.
 * `notes` follow whichever is shown.
 */
export type Shown =
  | { value: string; label?: undefined; notes: readonly string[] }
  | { value: null; label: string; notes: readonly string[] }

export function shownValue(
  value: string,
  notes: readonly string[] = [],
): Shown {
  return { value, notes }
}

export function shownLabel(
  label: string,
  notes: readonly string[] = [],
): Shown {
  return { value: null, label, notes }
}

/** The common shape of every `CachedRoomMetadata_*` snapshot. */
interface Snapshot<C> {
  status: Schemas['RoomMetadataStatus']
  content?: C | null
  redacted?: boolean | null
  invalid_fields: readonly string[]
  absence_confirmed_at?: number | null
  absence_check?: DetailRefreshDto | null
  sender?: string | null
  origin_ts?: number | null
}

/** The snapshot types the panel shows; `power_levels` is not one (decision 5). */
export type ShownSnapshot =
  | 'encryption'
  | 'server_acl'
  | 'join_rules'
  | 'history_visibility'
  | 'guest_access'
  | 'aliases'
  | 'creation'

/**
 * What the specification says a room without the state event is like
 * (decision 3, confirmed-absence table). Each names the setting it stands in
 * for, so a confirmed absence never reads as an explicit choice. `creation`
 * has none: every room has a create event, so its absence falls to row 11.
 */
const CONFIRMED_ABSENT: Readonly<Record<ShownSnapshot, string | null>> = {
  encryption: 'Not encrypted',
  server_acl: 'No server ACL (no servers denied)',
  join_rules: 'Invite only (no join rules set)',
  history_visibility: 'Shared (no history visibility set)',
  guest_access: 'Forbidden (no guest access set)',
  aliases: 'None',
  creation: null,
}

export const UNKNOWN_NOT_CACHED = 'Unknown (not cached)'
export const UNKNOWN_COULD_NOT_CHECK = "Unknown (couldn't check)"
export const CHECKING = 'Checking…'
export const WITHHELD = 'Withheld (malformed)'
export const REDACTED_NOTE = 'redacted; retained fields shown'
export const SOME_ENTRIES_WITHHELD = 'some entries withheld'

/**
 * A snapshot either has content the row formats (rows 1, 3 and 4), with any
 * notes that apply to all of it, or one label for the whole row.
 */
interface WithContent<C> {
  content: C
  notes: readonly string[]
  snapshot: Snapshot<C>
}

export type SnapshotView<C> = WithContent<C> | { content: null; label: string }

function refreshBusy(refresh: DetailRefreshDto | null | undefined): boolean {
  return refresh?.state === 'queued' || refresh?.state === 'running'
}

/** An idle refresh that failed, or is held back by a future backoff. */
function refreshHeld(
  refresh: DetailRefreshDto | null | undefined,
  now: number,
): boolean {
  if (refresh === null || refresh === undefined || refresh.state !== 'idle') {
    return false
  }
  return (
    (refresh.failure !== null && refresh.failure !== undefined) ||
    (typeof refresh.next_attempt_at === 'number' &&
      refresh.next_attempt_at > now)
  )
}

/**
 * Decision 3's metadata table, tested in order; the first match wins, so a
 * snapshot matching several rows (a redaction without content whose status is
 * `unavailable`, a confirmed absence whose re-check is `queued`) has one label.
 */
export function snapshotView<C>(
  snapshot: Snapshot<C>,
  type: ShownSnapshot,
  now: number,
): SnapshotView<C> {
  const content = snapshot.content ?? null
  // 1 and 2: redaction evidence outranks the status.
  if (snapshot.redacted === true) {
    return content !== null
      ? { content, notes: [REDACTED_NOTE], snapshot }
      : { content: null, label: 'Redacted' }
  }
  switch (snapshot.status) {
    // 3 and 4. `partial` content carries `invalid_fields`; the field
    // formatters below mark what was withheld.
    case 'available':
    case 'partial':
      return content !== null
        ? { content, notes: [], snapshot }
        : { content: null, label: UNKNOWN_NOT_CACHED }
    case 'unavailable':
      return { content: null, label: 'Unavailable (content not retained)' }
    case 'invalid':
      return { content: null, label: 'Unavailable (malformed state)' }
    case 'too_large':
      return { content: null, label: 'Unavailable (state too large)' }
  }
  // `unknown`, or a status this client predates: no tuple to show.
  if (
    snapshot.status === 'unknown' &&
    typeof snapshot.absence_confirmed_at === 'number'
  ) {
    // 8, whatever `absence_check` says.
    return {
      content: null,
      label: CONFIRMED_ABSENT[type] ?? UNKNOWN_NOT_CACHED,
    }
  }
  if (snapshot.status === 'unknown' && refreshBusy(snapshot.absence_check)) {
    return { content: null, label: CHECKING }
  }
  if (
    snapshot.status === 'unknown' &&
    refreshHeld(snapshot.absence_check, now)
  ) {
    return { content: null, label: UNKNOWN_COULD_NOT_CHECK }
  }
  return { content: null, label: UNKNOWN_NOT_CACHED }
}

function withheld(snapshot: Snapshot<unknown>, path: string): boolean {
  return snapshot.invalid_fields.includes(path)
}

/**
 * One optional scalar inside content: its value, "Withheld (malformed)" when
 * the server withheld it, or `unset` when the event omitted it. No Matrix
 * default is substituted for a missing field (ADR 0111).
 */
function scalar<C>(
  view: WithContent<C>,
  path: string,
  value: string | null | undefined,
  unset: string,
): Shown {
  if (typeof value === 'string') return shownValue(value, view.notes)
  return shownLabel(
    withheld(view.snapshot, path) ? WITHHELD : unset,
    view.notes,
  )
}

function labelOf(view: { label: string }): Shown {
  return shownLabel(view.label)
}

type EncryptionContent = NonNullable<RoomMetadataDto['encryption']['content']>
type JoinRulesContent = NonNullable<RoomMetadataDto['join_rules']['content']>
type AliasesContent = NonNullable<RoomMetadataDto['aliases']['content']>
type CreationContent = NonNullable<RoomMetadataDto['creation']['content']>
type ServerAclContent = NonNullable<RoomMetadataDto['server_acl']['content']>

/** Encryption is never inferred from a missing tuple (ADR 0126). */
export function encryptionShown(metadata: RoomMetadataDto, now: number): Shown {
  const view = snapshotView<EncryptionContent>(
    metadata.encryption,
    'encryption',
    now,
  )
  if (view.content === null) return labelOf(view)
  return scalar(view, 'algorithm', view.content.algorithm, 'Algorithm not set')
}

/**
 * The join rule, with the rooms whose members may join for `restricted` and
 * `knock_restricted` (decision 5). `roomName` resolves a room ID to a title
 * the reader knows, or returns the ID itself.
 */
export function accessShown(
  metadata: RoomMetadataDto,
  now: number,
  roomName: (roomId: string) => string,
): Shown {
  const view = snapshotView<JoinRulesContent>(
    metadata.join_rules,
    'join_rules',
    now,
  )
  if (view.content === null) return labelOf(view)
  const rule = scalar(view, 'join_rule', view.content.join_rule, 'Not set')
  const joinRule = view.content.join_rule
  if (joinRule !== 'restricted' && joinRule !== 'knock_restricted') return rule
  const conditions = (view.content.allow ?? []).map((condition) =>
    condition.type === 'm.room_membership' &&
    typeof condition.room_id === 'string'
      ? `members of ${roomName(condition.room_id)}`
      : condition.type,
  )
  if (withheld(view.snapshot, 'allow') || withheld(view.snapshot, 'allow[]')) {
    conditions.push('some conditions withheld')
  }
  return conditions.length === 0
    ? rule
    : { ...rule, notes: [...rule.notes, conditions.join('; ')] }
}

export function historyVisibilityShown(
  metadata: RoomMetadataDto,
  now: number,
): Shown {
  const view = snapshotView(
    metadata.history_visibility,
    'history_visibility',
    now,
  )
  if (view.content === null) return labelOf(view)
  return scalar(
    view,
    'history_visibility',
    view.content.history_visibility,
    'Not set',
  )
}

export function guestAccessShown(
  metadata: RoomMetadataDto,
  now: number,
): Shown {
  const view = snapshotView(metadata.guest_access, 'guest_access', now)
  if (view.content === null) return labelOf(view)
  return scalar(view, 'guest_access', view.content.guest_access, 'Not set')
}

/**
 * The canonical alias row (decision 4). Before `/metadata` arrives the panel
 * shows the room-list summary's alias instead; this is the answer once it has.
 */
export function canonicalAliasShown(
  metadata: RoomMetadataDto,
  now: number,
  summaryAlias: string | null = null,
): Shown {
  const view = snapshotView<AliasesContent>(metadata.aliases, 'aliases', now)
  if (view.content === null) {
    // A snapshot with no answer contradicts nothing, so the room-list
    // summary's alias stands in for it (decision 4). A redaction, a confirmed
    // absence or withheld state is an answer, and keeps its label.
    return summaryAlias !== null && snapshotSilent(metadata.aliases)
      ? shownValue(summaryAlias)
      : labelOf(view)
  }
  return scalar(view, 'alias', view.content.alias, 'None')
}

/**
 * Whether a snapshot says nothing about its state event: no content, not
 * redacted, not withheld as malformed or too large, and no confirmed absence.
 * That is `unknown` (however its check stands) and `unavailable`.
 */
function snapshotSilent(snapshot: Snapshot<unknown>): boolean {
  if (snapshot.redacted === true || (snapshot.content ?? null) !== null) {
    return false
  }
  if (snapshot.status === 'invalid' || snapshot.status === 'too_large') {
    return false
  }
  return typeof snapshot.absence_confirmed_at !== 'number'
}

/**
 * "Advertised aliases": `alt_aliases` only, as the TUI's `alias_lines` does,
 * since the canonical alias has its own row. "None" when the list is empty or
 * absent — never hidden, so it cannot be confused with a row still loading.
 */
export function advertisedAliasesShown(
  metadata: RoomMetadataDto,
  now: number,
): Shown {
  const view = snapshotView<AliasesContent>(metadata.aliases, 'aliases', now)
  if (view.content === null) return labelOf(view)
  const notes = withheld(view.snapshot, 'alt_aliases[]')
    ? [...view.notes, SOME_ENTRIES_WITHHELD]
    : view.notes
  const aliases = view.content.alt_aliases
  if (aliases !== null && aliases !== undefined && aliases.length > 0) {
    return shownValue(aliases.join(', '), notes)
  }
  if (
    (aliases === null || aliases === undefined) &&
    withheld(view.snapshot, 'alt_aliases')
  ) {
    return shownLabel(WITHHELD, notes)
  }
  return shownLabel('None', notes)
}

export function roomVersionShown(
  metadata: RoomMetadataDto,
  now: number,
): Shown {
  const view = snapshotView<CreationContent>(metadata.creation, 'creation', now)
  if (view.content === null) return labelOf(view)
  return scalar(view, 'room_version', view.content.room_version, 'Not recorded')
}

/**
 * `content.creator` when present, otherwise the create event's `sender` — room
 * versions from 11 drop the field and the sender is the creator
 * (`docs/room-metadata.md`, the TUI's `creation_lines`) — then any
 * `additional_creators`.
 */
export function creatorShown(metadata: RoomMetadataDto, now: number): Shown {
  const view = snapshotView<CreationContent>(metadata.creation, 'creation', now)
  if (view.content === null) return labelOf(view)
  const creator = view.content.creator ?? view.snapshot.sender ?? null
  const notes =
    withheld(view.snapshot, 'additional_creators') ||
    withheld(view.snapshot, 'additional_creators[]')
      ? [...view.notes, 'some creators withheld']
      : view.notes
  const others = view.content.additional_creators ?? []
  if (creator === null) {
    return shownLabel(
      withheld(view.snapshot, 'creator') ? WITHHELD : 'Not recorded',
      notes,
    )
  }
  return shownValue([creator, ...others].join(', '), notes)
}

/** For `m.room.create`, the event's `origin_ts` is when the room was made. */
export function createdShown(
  metadata: RoomMetadataDto,
  now: number,
  formatTime: (ts: number) => string,
): Shown {
  const view = snapshotView<CreationContent>(metadata.creation, 'creation', now)
  if (view.content === null) return labelOf(view)
  const ts = view.snapshot.origin_ts
  return typeof ts === 'number'
    ? shownValue(formatTime(ts), view.notes)
    : shownLabel('Not recorded', view.notes)
}

export function predecessorShown(
  metadata: RoomMetadataDto,
  now: number,
): Shown {
  const view = snapshotView<CreationContent>(metadata.creation, 'creation', now)
  if (view.content === null) return labelOf(view)
  const predecessor = view.content.predecessor?.room_id
  if (typeof predecessor === 'string')
    return shownValue(predecessor, view.notes)
  return shownLabel(
    withheld(view.snapshot, 'predecessor') ? WITHHELD : 'None',
    view.notes,
  )
}

/**
 * Counts of allowed and denied servers, and IP literals. An omitted field
 * reads "not specified", as in the TUI: the API preserves omission, so the
 * panel does not substitute the spec's default for it (decision 5).
 */
export function serverAclShown(metadata: RoomMetadataDto, now: number): Shown {
  const view = snapshotView<ServerAclContent>(
    metadata.server_acl,
    'server_acl',
    now,
  )
  if (view.content === null) return labelOf(view)
  const list = (
    name: 'allow' | 'deny',
    entries: string[] | null | undefined,
  ) =>
    Array.isArray(entries)
      ? `${entries.length}`
      : withheld(view.snapshot, name)
        ? 'withheld'
        : 'not specified'
  const ipLiterals =
    view.content.allow_ip_literals === true
      ? 'allowed'
      : view.content.allow_ip_literals === false
        ? 'denied'
        : withheld(view.snapshot, 'allow_ip_literals')
          ? 'withheld'
          : 'not specified'
  const notes =
    withheld(view.snapshot, 'allow[]') || withheld(view.snapshot, 'deny[]')
      ? [...view.notes, SOME_ENTRIES_WITHHELD]
      : view.notes
  return shownValue(
    `${list('allow', view.content.allow)} allowed · ${list('deny', view.content.deny)} denied · IP literals ${ipLiterals}`,
    notes,
  )
}

/**
 * The homeserver-details notes (decision 3, second table), for a value the
 * homeserver gave (rows 1–6): partial, then refresh in progress, else stale,
 * then a failed refresh. A refresh in progress replaces the staleness note
 * rather than adding to it.
 */
function homeserverNotes(
  detail: LocalAliasesDetailDto | DirectoryVisibilityDetailDto,
  now: number,
): string[] {
  const notes: string[] = []
  if (detail.status === 'partial') notes.push(SOME_ENTRIES_WITHHELD)
  const busy = refreshBusy(detail.refresh)
  if (busy) notes.push('Refreshing…')
  if (
    !busy &&
    typeof detail.expires_at === 'number' &&
    detail.expires_at < now
  ) {
    notes.push('may be out of date')
  }
  if (
    detail.refresh.state === 'idle' &&
    detail.refresh.failure !== null &&
    detail.refresh.failure !== undefined
  ) {
    notes.push("couldn't refresh")
  }
  return notes
}

/** Rows 3–9 of the homeserver-details value table; `null` for rows 1–2. */
function homeserverLabel(
  detail: LocalAliasesDetailDto | DirectoryVisibilityDetailDto,
  now: number,
): Shown | null {
  switch (detail.status) {
    case 'available':
    case 'partial':
      return null
    case 'too_large':
      return shownLabel('Too many to show', homeserverNotes(detail, now))
    case 'forbidden':
      return shownLabel(
        "Your homeserver didn't allow this lookup",
        homeserverNotes(detail, now),
      )
    case 'not_found':
      return shownLabel(
        "Your homeserver doesn't know this room",
        homeserverNotes(detail, now),
      )
    case 'unsupported':
      return shownLabel(
        'Not supported by your homeserver',
        homeserverNotes(detail, now),
      )
  }
  // `unknown`, or a status this client predates. Rows 7–9 already describe
  // the refresh, so they carry no notes.
  if (refreshBusy(detail.refresh)) return shownLabel(CHECKING)
  if (refreshHeld(detail.refresh, now))
    return shownLabel(UNKNOWN_COULD_NOT_CHECK)
  return shownLabel(UNKNOWN_NOT_CACHED)
}

/** "Aliases on your homeserver": `local_aliases` (decision 4). */
export function localAliasesShown(
  details: RoomHomeserverDetailsDto,
  now: number,
): Shown {
  const detail = details.local_aliases
  const label = homeserverLabel(detail, now)
  if (label !== null) return label
  const notes = homeserverNotes(detail, now)
  const aliases = detail.aliases ?? []
  return aliases.length === 0
    ? shownLabel('None', notes)
    : shownValue(aliases.join(', '), notes)
}

export function directoryVisibilityShown(
  details: RoomHomeserverDetailsDto,
  now: number,
): Shown {
  const detail = details.directory_visibility
  const label = homeserverLabel(detail, now)
  if (label !== null) return label
  const notes = homeserverNotes(detail, now)
  if (detail.visibility === 'public') return shownValue('Public', notes)
  if (detail.visibility === 'private') return shownValue('Private', notes)
  return shownLabel(UNKNOWN_NOT_CACHED, notes)
}

/**
 * "N joined · M invited". `member_counts: null` is "not yet observed", never
 * zero; `observed_at` is when the pair last changed locally, not a freshness
 * time, so it is not shown as one.
 */
export function memberCountsShown(info: RoomInfoDto): Shown {
  const counts = info.member_counts
  if (counts === null || counts === undefined) {
    return shownLabel('Unknown (not yet observed)')
  }
  return shownValue(`${counts.joined} joined · ${counts.invited} invited`)
}

/**
 * Why the member list is shorter than the Members count, or `null` when it is
 * not (or the count is unknown). `GET …/members` returns only the members sync
 * has delivered — Matrix loads them lazily — so a large room lists a handful
 * while `member_counts` reports hundreds. The complete list is issue 659's
 * server work; until then the panel says the list is partial rather than let
 * it pass for everyone.
 */
export function memberListCoverage(
  loadedJoined: number,
  info: RoomInfoDto | null,
): string | null {
  const joined = info?.member_counts?.joined
  if (typeof joined !== 'number' || loadedJoined >= joined) return null
  const noun = joined === 1 ? 'member' : 'members'
  return `Showing ${loadedJoined} of ${joined} joined ${noun}: Axon lists only the members your homeserver has sent it so far.`
}

export type DiscoveredRoomDto = Schemas['DiscoveredRoomDto']
export type SpaceDiscoveryDto = Schemas['SpaceDiscoveryDto']
export type SpaceHierarchyChildDto = Schemas['SpaceHierarchyChildDto']

export const NOT_LOOKED_UP = 'Retrieving'
export const NOT_VISIBLE = 'Not in space directory'
export const FROM_SPACE_DIRECTORY = 'From space directory'

/**
 * The provenance note for a discovered summary (decision 3): where it came
 * from, and how long ago it was observed ("updated 10m"). Coarse on purpose:
 * minutes, hours or days are all a reader needs to judge it.
 */
export function discoveredProvenance(
  discovered: DiscoveredRoomDto,
  now: number = Date.now(),
): string {
  return `${FROM_SPACE_DIRECTORY} · updated ${relativeTime(discovered.observed_at, now)}`
}

/**
 * One unjoined room's preview rows (decisions 3 and 8), or one label for the
 * whole preview when there is no summary to show. A withheld field is left
 * out rather than shown as empty; a field the answer did not carry reads
 * "None" where Matrix has no such state (no name, topic or alias), and
 * "Unknown" where the homeserver simply did not say: `encryption: null` in
 * particular is never "not encrypted" (homeservers before Matrix v1.15 do not
 * report it).
 */
export function discoveredPreview(
  discovered: DiscoveredRoomDto | null | undefined,
  now: number = Date.now(),
):
  | { rows: ReadonlyArray<{ label: string; shown: Shown }>; note: string }
  | { rows: null; label: string } {
  if (discovered === null || discovered === undefined) {
    return { rows: null, label: NOT_LOOKED_UP }
  }
  if (discovered.status !== 'available') {
    return { rows: null, label: NOT_VISIBLE }
  }
  const withheldFields = new Set(discovered.withheld)
  const rows: Array<{ label: string; shown: Shown }> = []
  const add = (
    label: string,
    field: keyof DiscoveredRoomDto & string,
    value: string | null,
    missing: string,
  ) => {
    if (withheldFields.has(field)) return
    rows.push({
      label,
      shown: value === null ? shownLabel(missing) : shownValue(value),
    })
  }
  add('Name', 'name', nonEmpty(discovered.name), 'None')
  add('Topic', 'topic', nonEmpty(discovered.topic), 'None')
  add(
    'Members',
    'num_joined_members',
    typeof discovered.num_joined_members === 'number'
      ? `${discovered.num_joined_members} joined`
      : null,
    'Unknown',
  )
  add('Join rule', 'join_rule', nonEmpty(discovered.join_rule), 'Unknown')
  add('Encryption', 'encryption', nonEmpty(discovered.encryption), 'Unknown')
  add(
    'Room version',
    'room_version',
    nonEmpty(discovered.room_version),
    'Unknown',
  )
  add(
    'Canonical alias',
    'canonical_alias',
    nonEmpty(discovered.canonical_alias),
    'None',
  )
  return { rows, note: discoveredProvenance(discovered, now) }
}

function nonEmpty(value: string | null | undefined): string | null {
  return typeof value === 'string' && value !== '' ? value : null
}

/**
 * Which source a space child's name, avatar and topic come from (ADR 0128
 * precedence, decision 6): synced fields when the account has joined it;
 * otherwise an `available` discovered summary; otherwise synced fields.
 * The two sources are never merged, so a field the chosen source lacks
 * falls back to the room ID, not to the other source.
 */
export function spaceChildDisplay(
  child: SpaceHierarchyChildDto,
  now: number = Date.now(),
): {
  name: string
  topic: string | null
  avatarUrl: string | null
  /** Joined state, then where the fields came from or why there are none. */
  notes: readonly string[]
} {
  const discovered = child.discovered ?? null
  if (child.joined) {
    return {
      name: nonEmpty(child.name) ?? child.room_id,
      topic: null,
      avatarUrl: nonEmpty(child.avatar_url),
      notes: ['Joined'],
    }
  }
  if (discovered !== null && discovered.status === 'available') {
    return {
      name: nonEmpty(discovered.name) ?? child.room_id,
      topic: nonEmpty(discovered.topic),
      avatarUrl: nonEmpty(discovered.avatar_url),
      notes: ['Not joined', discoveredProvenance(discovered, now)],
    }
  }
  return {
    name: nonEmpty(child.name) ?? child.room_id,
    topic: null,
    avatarUrl: nonEmpty(child.avatar_url),
    notes: ['Not joined', discovered === null ? NOT_LOOKED_UP : NOT_VISIBLE],
  }
}

/** "Showing N of T", counting the rows actually loaded (decision 6). */
export function hierarchyCountShown(loaded: number, total: number): string {
  return `Showing ${loaded.toLocaleString()} of ${total.toLocaleString()}`
}

/**
 * Decision 6's progress table, tested in order (every unsuccessful crawl is
 * `outcome: failed`, so the conclusive failures come before the transient
 * one). `null` is row 8: nothing beyond "Showing N of T".
 */
export function crawlProgressShown(
  discovery: SpaceDiscoveryDto,
  formatTime: (ts: number) => string,
): string | null {
  if (discovery.state === 'queued' || discovery.state === 'running') {
    return `Looking up rooms… (${discovery.rooms_cached} found)`
  }
  if (discovery.support === 'unsupported') {
    return "Your homeserver can't list this space's rooms"
  }
  if (discovery.outcome === 'failed') {
    if (discovery.failure === 'forbidden') {
      return "Your homeserver didn't allow listing this space's rooms"
    }
    if (discovery.failure === 'not_found') {
      return "Your homeserver couldn't find this space's room list"
    }
    return typeof discovery.next_attempt_at === 'number'
      ? `Couldn't look up rooms; trying again at ${formatTime(discovery.next_attempt_at)}`
      : "Couldn't look up rooms"
  }
  if (discovery.outcome === 'in_progress') {
    return 'Room lookup paused; resuming…'
  }
  if (discovery.outcome === 'truncated') {
    return 'The space directory listed only part of this space'
  }
  return null
}
