import { describe, expect, it } from 'vitest'
import {
  homeserverDetailsDto,
  metadataDto,
  snapshot,
  unknownSnapshot,
} from '../test/room-details'
import {
  accessShown,
  advertisedAliasesShown,
  canonicalAliasShown,
  createdShown,
  creatorShown,
  directoryVisibilityShown,
  encryptionShown,
  guestAccessShown,
  historyVisibilityShown,
  localAliasesShown,
  memberCountsShown,
  memberListCoverage,
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
