import { describe, expect, it } from 'vitest'
import {
  discoveredRoom,
  hierarchyChild,
  homeserverDetailsDto,
  metadataDto,
  snapshot,
  spaceDiscovery,
  unknownSnapshot,
} from '../test/room-details'
import {
  accessShown,
  advertisedAliasesShown,
  canonicalAliasShown,
  crawlProgressShown,
  createdShown,
  creatorShown,
  directoryVisibilityShown,
  discoveredPreview,
  encryptionShown,
  guestAccessShown,
  hierarchyCountShown,
  historyVisibilityShown,
  localAliasesShown,
  memberCountsShown,
  memberListCoverage,
  spaceChildDisplay,
  predecessorShown,
  roomVersionShown,
  serverAclShown,
  type Shown,
} from './room-details-format'

const NOW = 1_800_000_000_000
const FUTURE = NOW + 60_000
const PAST = NOW - 60_000

/** What the row shows: the value, or the label in its place, then the notes. */
const text = (shown: Shown) =>
  [shown.value ?? shown.label, ...shown.notes].join(' · ')

describe('metadata snapshots (ADR 0133 decision 3, in order)', () => {
  const encryption = (snap: unknown) =>
    encryptionShown(metadataDto({ encryption: snap }), NOW)

  it('row 1: redacted with content shows the retained fields, noted', () => {
    const shown = encryption(
      snapshot(
        { algorithm: 'm.megolm.v1.aes-sha2' },
        { redacted: true, status: 'unavailable' },
      ),
    )
    expect(shown.value).toBe('m.megolm.v1.aes-sha2')
    expect(shown.notes).toEqual(['redacted; retained fields shown'])
  })

  it('row 2: redacted without content is "Redacted", whatever the status', () => {
    // Matches rows 2 and 5; the order settles it.
    expect(
      text(
        encryption(unknownSnapshot({ status: 'unavailable', redacted: true })),
      ),
    ).toBe('Redacted')
  })

  it('row 3: available shows the value, and it is copyable data', () => {
    const shown = encryption(snapshot({ algorithm: 'm.megolm.v1.aes-sha2' }))
    expect(shown).toEqual({ value: 'm.megolm.v1.aes-sha2', notes: [] })
  })

  it('row 4: partial shows valid fields and marks the withheld one', () => {
    expect(
      text(
        encryption(
          snapshot(
            { algorithm: null },
            { status: 'partial', invalid_fields: ['algorithm'] },
          ),
        ),
      ),
    ).toBe('Withheld (malformed)')
    const aliases = advertisedAliasesShown(
      metadataDto({
        aliases: snapshot(
          { alias: '#a:hs', alt_aliases: ['#b:hs'] },
          { status: 'partial', invalid_fields: ['alt_aliases[]'] },
        ),
      }),
      NOW,
    )
    expect(text(aliases)).toBe('#b:hs · some entries withheld')
  })

  it.each([
    ['unavailable', 'Unavailable (content not retained)'],
    ['invalid', 'Unavailable (malformed state)'],
    ['too_large', 'Unavailable (state too large)'],
  ])('rows 5–7: %s', (status, label) => {
    expect(text(encryption(unknownSnapshot({ status })))).toBe(label)
  })

  it('row 8: a confirmed absence wins over a queued re-check', () => {
    expect(
      text(
        encryption(
          unknownSnapshot({
            absence_confirmed_at: PAST,
            absence_check: { state: 'queued' },
          }),
        ),
      ),
    ).toBe('Not encrypted')
  })

  it.each(['queued', 'running'])(
    'row 9: a %s check is "Checking…"',
    (state) => {
      expect(
        text(encryption(unknownSnapshot({ absence_check: { state } }))),
      ).toBe('Checking…')
    },
  )

  it('row 10: an idle check that failed or is backing off', () => {
    expect(
      text(
        encryption(
          unknownSnapshot({
            absence_check: { state: 'idle', failure: 'timeout' },
          }),
        ),
      ),
    ).toBe("Unknown (couldn't check)")
    expect(
      text(
        encryption(
          unknownSnapshot({
            absence_check: { state: 'idle', next_attempt_at: FUTURE },
          }),
        ),
      ),
    ).toBe("Unknown (couldn't check)")
  })

  it('row 11: otherwise unknown — never "Not encrypted"', () => {
    expect(text(encryption(unknownSnapshot()))).toBe('Unknown (not cached)')
    // A backoff that has already passed is no longer holding the check.
    expect(
      text(
        encryption(
          unknownSnapshot({
            absence_check: { state: 'idle', next_attempt_at: PAST },
          }),
        ),
      ),
    ).toBe('Unknown (not cached)')
    expect(text(encryption(unknownSnapshot({ absence_check: null })))).toBe(
      'Unknown (not cached)',
    )
  })
})

describe('confirmed-absence defaults', () => {
  const absent = unknownSnapshot({ absence_confirmed_at: PAST })
  const all = metadataDto({
    encryption: absent,
    server_acl: absent,
    join_rules: absent,
    history_visibility: absent,
    guest_access: absent,
    aliases: absent,
    creation: absent,
  })

  it('names the setting each default stands in for', () => {
    expect(text(encryptionShown(all, NOW))).toBe('Not encrypted')
    expect(text(serverAclShown(all, NOW))).toBe(
      'No server ACL (no servers denied)',
    )
    expect(text(accessShown(all, NOW, (id) => id))).toBe(
      'Invite only (no join rules set)',
    )
    expect(text(historyVisibilityShown(all, NOW))).toBe(
      'Shared (no history visibility set)',
    )
    expect(text(guestAccessShown(all, NOW))).toBe(
      'Forbidden (no guest access set)',
    )
    expect(text(canonicalAliasShown(all, NOW))).toBe('None')
    expect(text(advertisedAliasesShown(all, NOW))).toBe('None')
  })

  it('gives creation no default: every room has a create event', () => {
    expect(text(roomVersionShown(all, NOW))).toBe('Unknown (not cached)')
    expect(text(creatorShown(all, NOW))).toBe('Unknown (not cached)')
  })

  it('never makes a default copyable', () => {
    expect(encryptionShown(all, NOW).value).toBeNull()
  })
})

describe('row formatters', () => {
  it('access lists who may join a restricted room', () => {
    const metadata = metadataDto({
      join_rules: snapshot({
        join_rule: 'restricted',
        allow: [
          { type: 'm.room_membership', room_id: '!space:hs' },
          { type: 'org.example.other' },
        ],
      }),
    })
    expect(
      text(
        accessShown(metadata, NOW, (id) => (id === '!space:hs' ? 'Ops' : id)),
      ),
    ).toBe('restricted · members of Ops; org.example.other')
  })

  it('access does not list conditions for an unrestricted rule', () => {
    const metadata = metadataDto({
      join_rules: snapshot({
        join_rule: 'invite',
        allow: [{ type: 'm.room_membership', room_id: '!space:hs' }],
      }),
    })
    expect(text(accessShown(metadata, NOW, (id) => id))).toBe('invite')
  })

  it('advertised aliases are alt_aliases only, "None" when empty or absent', () => {
    const shown = (content: unknown) =>
      text(
        advertisedAliasesShown(
          metadataDto({ aliases: snapshot(content) }),
          NOW,
        ),
      )
    expect(shown({ alias: '#a:hs', alt_aliases: ['#b:hs', '#c:hs'] })).toBe(
      '#b:hs, #c:hs',
    )
    expect(shown({ alias: '#a:hs', alt_aliases: [] })).toBe('None')
    expect(shown({ alias: '#a:hs' })).toBe('None')
  })

  it('the canonical alias row uses the snapshot label without content', () => {
    expect(
      text(
        canonicalAliasShown(
          metadataDto({
            aliases: unknownSnapshot({ absence_check: { state: 'running' } }),
          }),
          NOW,
        ),
      ),
    ).toBe('Checking…')
    expect(
      text(
        canonicalAliasShown(
          metadataDto({ aliases: snapshot({ alt_aliases: ['#b:hs'] }) }),
          NOW,
        ),
      ),
    ).toBe('None')
  })

  it('creator falls back to the create event sender, then lists additional creators', () => {
    const creation = (content: unknown, extra = {}) =>
      metadataDto({ creation: snapshot(content, extra) })
    expect(
      text(
        creatorShown(creation({ creator: '@old:hs', room_version: '9' }), NOW),
      ),
    ).toBe('@old:hs')
    expect(
      text(
        creatorShown(
          creation(
            { room_version: '12', additional_creators: ['@b:hs'] },
            { sender: '@a:hs' },
          ),
          NOW,
        ),
      ),
    ).toBe('@a:hs, @b:hs')
  })

  it('creation time is the create event origin_ts; version and predecessor read content', () => {
    const metadata = metadataDto({
      creation: snapshot(
        { room_version: '11', predecessor: { room_id: '!old:hs' } },
        { origin_ts: 42 },
      ),
    })
    expect(text(createdShown(metadata, NOW, (ts) => `at ${ts}`))).toBe('at 42')
    expect(text(roomVersionShown(metadata, NOW))).toBe('11')
    expect(text(predecessorShown(metadata, NOW))).toBe('!old:hs')
    expect(text(predecessorShown(metadataDto(), NOW))).toBe('None')
  })

  it('server ACL summarizes counts and keeps omission as "not specified"', () => {
    const acl = (content: unknown) =>
      text(serverAclShown(metadataDto({ server_acl: snapshot(content) }), NOW))
    expect(
      acl({
        allow: ['*'],
        deny: ['bad.example', 'worse.example'],
        allow_ip_literals: false,
      }),
    ).toBe('1 allowed · 2 denied · IP literals denied')
    expect(acl({ allow: ['*'], allow_ip_literals: true })).toBe(
      '1 allowed · not specified denied · IP literals allowed',
    )
    expect(acl({ deny: [] })).toBe(
      'not specified allowed · 0 denied · IP literals not specified',
    )
  })

  it('member counts: null is "not yet observed", never zero', () => {
    expect(
      text(
        memberCountsShown({
          member_counts: { joined: 4, invited: 2, observed_at: 1 },
        }),
      ),
    ).toBe('4 joined · 2 invited')
    expect(text(memberCountsShown({ member_counts: null }))).toBe(
      'Unknown (not yet observed)',
    )
    expect(
      text(
        memberCountsShown({
          member_counts: { joined: 0, invited: 0, observed_at: 1 },
        }),
      ),
    ).toBe('0 joined · 0 invited')
  })
})

describe('homeserver details (decision 3, value and note tables)', () => {
  const aliases = (detail: Record<string, unknown>) =>
    text(
      localAliasesShown(
        homeserverDetailsDto({
          local_aliases: {
            fetched_at: PAST,
            expires_at: FUTURE,
            refresh: { state: 'idle' },
            ...detail,
          },
        }),
        NOW,
      ),
    )
  const visibility = (detail: Record<string, unknown>) =>
    text(
      directoryVisibilityShown(
        homeserverDetailsDto({
          directory_visibility: {
            fetched_at: PAST,
            expires_at: FUTURE,
            refresh: { state: 'idle' },
            ...detail,
          },
        }),
        NOW,
      ),
    )

  it('row 1: available', () => {
    expect(aliases({ status: 'available', aliases: ['#x:hs', '#y:hs'] })).toBe(
      '#x:hs, #y:hs',
    )
    expect(aliases({ status: 'available', aliases: [] })).toBe('None')
    expect(visibility({ status: 'available', visibility: 'public' })).toBe(
      'Public',
    )
    expect(visibility({ status: 'available', visibility: 'private' })).toBe(
      'Private',
    )
  })

  it('row 2 + note a: partial', () => {
    expect(aliases({ status: 'partial', aliases: ['#x:hs'] })).toBe(
      '#x:hs · some entries withheld',
    )
  })

  it.each([
    ['too_large', 'Too many to show'],
    ['forbidden', "Your homeserver didn't allow this lookup"],
    ['not_found', "Your homeserver doesn't know this room"],
    ['unsupported', 'Not supported by your homeserver'],
  ])('rows 3–6: %s', (status, label) => {
    expect(aliases({ status, aliases: null })).toBe(label)
  })

  it('rows 7–9: unknown describes the refresh, with no notes', () => {
    expect(aliases({ status: 'unknown', refresh: { state: 'queued' } })).toBe(
      'Checking…',
    )
    expect(aliases({ status: 'unknown', refresh: { state: 'running' } })).toBe(
      'Checking…',
    )
    expect(
      aliases({
        status: 'unknown',
        expires_at: PAST,
        refresh: { state: 'idle', failure: 'timeout' },
      }),
    ).toBe("Unknown (couldn't check)")
    expect(
      aliases({
        status: 'unknown',
        refresh: { state: 'idle', next_attempt_at: FUTURE },
      }),
    ).toBe("Unknown (couldn't check)")
    expect(aliases({ status: 'unknown', expires_at: null })).toBe(
      'Unknown (not cached)',
    )
  })

  it('a stale value stays visible next to a failed refresh', () => {
    expect(
      visibility({
        status: 'available',
        visibility: 'public',
        expires_at: PAST,
        refresh: { state: 'idle', failure: 'server_error' },
      }),
    ).toBe("Public · may be out of date · couldn't refresh")
  })

  it('a refresh in progress replaces the staleness note', () => {
    expect(
      visibility({
        status: 'available',
        visibility: 'public',
        expires_at: PAST,
        refresh: { state: 'running' },
      }),
    ).toBe('Public · Refreshing…')
  })

  it('notes follow a denial too, in table order', () => {
    expect(
      aliases({
        status: 'forbidden',
        aliases: null,
        expires_at: PAST,
        refresh: { state: 'idle', failure: 'transport' },
      }),
    ).toBe(
      "Your homeserver didn't allow this lookup · may be out of date · couldn't refresh",
    )
  })
})

describe('memberListCoverage', () => {
  const counts = (joined: number) => ({
    member_counts: { joined, invited: 0, observed_at: 1 },
  })

  it('explains a list shorter than the joined count', () => {
    expect(memberListCoverage(12, counts(340))).toBe(
      'Showing 12 of 340 joined members: Axon lists only the members your homeserver has sent it so far.',
    )
    expect(memberListCoverage(0, counts(1))).toBe(
      'Showing 0 of 1 joined member: Axon lists only the members your homeserver has sent it so far.',
    )
  })

  it('says nothing when the list is complete or the count unknown', () => {
    expect(memberListCoverage(3, counts(3))).toBeNull()
    // A stale count below the loaded list is not a gap either.
    expect(memberListCoverage(4, counts(3))).toBeNull()
    expect(memberListCoverage(3, { member_counts: null })).toBeNull()
    expect(memberListCoverage(3, null)).toBeNull()
  })
})

describe('the canonical alias with a room-summary alias to fall back on', () => {
  const shown = (aliases: unknown) =>
    text(
      canonicalAliasShown(
        metadataDto({ aliases: aliases as never }),
        NOW,
        '#summary:hs',
      ),
    )

  it('uses the summary alias where the snapshot has no answer', () => {
    expect(shown(unknownSnapshot())).toBe('#summary:hs')
    expect(
      shown(unknownSnapshot({ absence_check: { state: 'running' } })),
    ).toBe('#summary:hs')
    expect(shown(unknownSnapshot({ status: 'unavailable' }))).toBe(
      '#summary:hs',
    )
  })

  it('keeps the snapshot’s own answer wherever it has one', () => {
    expect(shown(snapshot({ alias: '#meta:hs' }))).toBe('#meta:hs')
    expect(shown(snapshot({}))).toBe('None')
    expect(
      shown(unknownSnapshot({ absence_confirmed_at: 1_700_000_000_000 })),
    ).toBe('None')
    expect(
      shown(unknownSnapshot({ status: 'unavailable', redacted: true })),
    ).toBe('Redacted')
    expect(shown(unknownSnapshot({ status: 'invalid' }))).toBe(
      'Unavailable (malformed state)',
    )
    expect(shown(unknownSnapshot({ status: 'too_large' }))).toBe(
      'Unavailable (state too large)',
    )
  })
})

const at = (ts: number) => `t${ts}`
/** Ten minutes after the fixtures' `observed_at`. */
const SEEN_NOW = 1_700_000_000_000 + 10 * 60_000

describe('discovered summaries (decision 3)', () => {
  it('says when a room has not been looked up, or was not visible', () => {
    expect(discoveredPreview(null, SEEN_NOW)).toEqual({
      rows: null,
      label: 'Retrieving',
    })
    for (const status of ['not_returned', 'forbidden', 'not_found'] as const) {
      expect(discoveredPreview(discoveredRoom({ status }), SEEN_NOW)).toEqual({
        rows: null,
        label: 'Not in space directory',
      })
    }
  })

  it('shows an available summary with its provenance', () => {
    const shown = discoveredPreview(
      discoveredRoom({ encryption: 'm.megolm.v1.aes-sha2' }),
      SEEN_NOW,
    )
    if (shown.rows === null) throw new Error('expected rows')
    expect(
      shown.rows.map((row) => [row.label, row.shown.value ?? row.shown.label]),
    ).toEqual([
      ['Name', 'Salmon count'],
      ['Topic', 'Counting fish'],
      ['Members', '4 joined'],
      ['Join rule', 'public'],
      ['Encryption', 'm.megolm.v1.aes-sha2'],
      ['Room version', '11'],
      ['Canonical alias', '#salmon-count:hs'],
    ])
    expect(shown.note).toBe('From space directory · updated 10m')
  })

  it('never reads a missing encryption as unencrypted, and leaves withheld fields out', () => {
    const shown = discoveredPreview(
      discoveredRoom({
        encryption: null,
        topic: null,
        num_joined_members: null,
        name: null,
        withheld: ['name'],
      }),
      SEEN_NOW,
    )
    if (shown.rows === null) throw new Error('expected rows')
    const byLabel = new Map(
      shown.rows.map((row) => [row.label, row.shown.label ?? row.shown.value]),
    )
    expect(byLabel.get('Encryption')).toBe('Unknown')
    expect(byLabel.get('Members')).toBe('Unknown')
    expect(byLabel.get('Topic')).toBe('None')
    expect(byLabel.has('Name')).toBe(false)
    // State labels are never copyable data.
    expect(
      shown.rows.find((row) => row.label === 'Encryption')?.shown.value,
    ).toBe(null)
  })
})

describe('space children display precedence (decision 6)', () => {
  it('uses synced fields for a joined child', () => {
    expect(
      spaceChildDisplay(
        hierarchyChild('!a:hs', {
          joined: true,
          name: 'Synced',
          discovered: discoveredRoom({ name: 'Directory' }),
        }),
        SEEN_NOW,
      ),
    ).toMatchObject({ name: 'Synced', notes: ['Joined'] })
  })

  it('uses an available summary for an unjoined child, never merging', () => {
    expect(
      spaceChildDisplay(
        hierarchyChild('!a:hs', {
          name: 'Synced',
          discovered: discoveredRoom({
            name: null,
            topic: 'T',
            observed_at: SEEN_NOW - 3 * 3_600_000,
          }),
        }),
        SEEN_NOW,
      ),
    ).toEqual({
      name: '!a:hs',
      topic: 'T',
      avatarUrl: null,
      notes: ['Not joined', 'From space directory · updated 3h'],
    })
  })

  it('falls back to synced fields, then the room ID', () => {
    expect(
      spaceChildDisplay(
        hierarchyChild('!a:hs', {
          name: 'Synced',
          discovered: discoveredRoom({ status: 'forbidden' }),
        }),
        SEEN_NOW,
      ),
    ).toMatchObject({
      name: 'Synced',
      notes: ['Not joined', 'Not in space directory'],
    })
    expect(spaceChildDisplay(hierarchyChild('!a:hs'), SEEN_NOW)).toMatchObject({
      name: '!a:hs',
      notes: ['Not joined', 'Retrieving'],
    })
  })
})

describe('crawl progress (decision 6)', () => {
  const failed = (extra: object) =>
    spaceDiscovery({ outcome: 'failed', ...extra })

  it.each([
    [
      'row 1: queued or running, whatever else it says',
      spaceDiscovery({
        state: 'queued',
        rooms_cached: 3,
        outcome: 'failed',
        support: 'unsupported',
      }),
      'Looking up rooms… (3 found)',
    ],
    [
      'row 2: unsupported, ahead of its failed outcome',
      failed({ support: 'unsupported', failure: 'not_found' }),
      "Your homeserver can't list this space's rooms",
    ],
    [
      'row 3: forbidden',
      failed({ failure: 'forbidden', next_attempt_at: 9 }),
      "Your homeserver didn't allow listing this space's rooms",
    ],
    [
      'row 4: not found',
      failed({ failure: 'not_found', next_attempt_at: 9 }),
      "Your homeserver couldn't find this space's room list",
    ],
    [
      'row 5: a transient failure',
      failed({ failure: 'rate_limited', next_attempt_at: 9 }),
      "Couldn't look up rooms; trying again at t9",
    ],
    [
      'row 6: paused',
      spaceDiscovery({ outcome: 'in_progress' }),
      'Room lookup paused; resuming…',
    ],
    [
      'row 7: truncated',
      spaceDiscovery({ outcome: 'truncated' }),
      'The space directory listed only part of this space',
    ],
    ['row 8: complete', spaceDiscovery({ outcome: 'complete' }), null],
    ['row 8: never crawled', spaceDiscovery({ outcome: null }), null],
  ])('%s', (_name, discovery, line) => {
    expect(crawlProgressShown(discovery, at)).toBe(line)
  })

  it('counts the rows actually loaded', () => {
    expect(hierarchyCountShown(50, 4864)).toBe(
      `Showing 50 of ${(4864).toLocaleString()}`,
    )
  })
})
