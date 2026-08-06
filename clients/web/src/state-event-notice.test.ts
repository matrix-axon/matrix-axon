import { describe, expect, it } from 'vitest'
import { stateEventNotice, stateEventTier } from './state-event-notice'
import type { EventDto } from './stores/timeline'

function memberEvent(fields: {
  content: Record<string, unknown> | null
  prev?: Record<string, unknown> | null
  sender?: string
  stateKey?: string
}): EventDto {
  return {
    account_id: '00000000-0000-0000-0000-000000000000',
    event_id: '$e:example.org',
    room_id: '!r:example.org',
    sender: fields.sender ?? '@alice:example.org',
    state_key: fields.stateKey ?? '@alice:example.org',
    content: fields.content,
    prev_content: fields.prev ?? null,
    origin_ts: 0,
    arrival_order: 0,
    type: 'm.room.member',
    body: null,
    redacted: false,
  } as unknown as EventDto
}

const bob = (userId: string) => (userId === '@bob:example.org' ? 'Bob' : userId)

describe('stateEventTier', () => {
  it('treats membership as important and everything else as other', () => {
    expect(stateEventTier(memberEvent({ content: {} }))).toBe('important')
    expect(
      stateEventTier({
        ...memberEvent({ content: {} }),
        type: 'm.room.topic',
      }),
    ).toBe('other')
  })
})

describe('stateEventNotice', () => {
  it('renders a join from any non-join previous membership', () => {
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'join', displayname: 'Alice' },
          prev: { membership: 'invite' },
        }),
      ),
    ).toBe('Alice joined the room')
    expect(
      stateEventNotice(
        memberEvent({ content: { membership: 'join', displayname: 'Alice' } }),
      ),
    ).toBe('Alice joined the room')
  })

  it('does not call a display-name change a join (issue #31)', () => {
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'join', displayname: 'Alice B' },
          prev: { membership: 'join', displayname: 'Alice' },
        }),
      ),
    ).toBe('Alice changed their display name to Alice B')
  })

  it('distinguishes setting and removing a display name', () => {
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'join', displayname: 'Alice' },
          prev: { membership: 'join' },
        }),
      ),
    ).toBe('@alice set their display name to Alice')
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'join' },
          prev: { membership: 'join', displayname: 'Alice' },
        }),
      ),
    ).toBe('Alice removed their display name')
  })

  it('reports an avatar change only when the display name held still', () => {
    expect(
      stateEventNotice(
        memberEvent({
          content: {
            membership: 'join',
            displayname: 'Alice',
            avatar_url: 'mxc://example.org/new',
          },
          prev: {
            membership: 'join',
            displayname: 'Alice',
            avatar_url: 'mxc://example.org/old',
          },
        }),
      ),
    ).toBe('Alice changed their profile picture')
  })

  it('suppresses a member event that changed nothing visible', () => {
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'join', displayname: 'Alice' },
          prev: { membership: 'join', displayname: 'Alice' },
        }),
      ),
    ).toBeNull()
  })

  it('separates leaving from being removed by someone else', () => {
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'leave' },
          prev: { membership: 'join', displayname: 'Alice' },
        }),
      ),
    ).toBe('Alice left the room')
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'leave' },
          prev: { membership: 'join', displayname: 'Alice' },
          sender: '@bob:example.org',
        }),
        bob,
      ),
    ).toBe('Bob removed Alice from the room')
  })

  it('reads invite, decline, withdrawal, ban and unban', () => {
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'invite' },
          sender: '@bob:example.org',
        }),
        bob,
      ),
    ).toBe('Bob invited @alice')
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'leave' },
          prev: { membership: 'invite', displayname: 'Alice' },
        }),
      ),
    ).toBe('Alice declined the invitation')
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'leave' },
          prev: { membership: 'invite', displayname: 'Alice' },
          sender: '@bob:example.org',
        }),
        bob,
      ),
    ).toBe('Bob withdrew the invitation to Alice')
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'ban', displayname: 'Alice' },
          sender: '@bob:example.org',
        }),
        bob,
      ),
    ).toBe('Bob banned Alice')
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'leave' },
          prev: { membership: 'ban', displayname: 'Alice' },
          sender: '@bob:example.org',
        }),
        bob,
      ),
    ).toBe('Bob unbanned Alice')
  })

  it('renders a knock and an unrecognized membership', () => {
    expect(
      stateEventNotice(
        memberEvent({ content: { membership: 'knock', displayname: 'Alice' } }),
      ),
    ).toBe('Alice asked to join the room')
    expect(
      stateEventNotice(memberEvent({ content: { membership: 'quantum' } })),
    ).toBe('@alice membership changed: quantum')
  })

  it('names a departed user from the event rather than the roster', () => {
    // The roster no longer has them, so `resolveName` echoes the raw id back —
    // the event's own `prev_content` is what still carries their name.
    const echo = (userId: string) => userId
    expect(
      stateEventNotice(
        memberEvent({
          content: { membership: 'leave' },
          prev: { membership: 'join', displayname: 'Alice' },
        }),
        echo,
      ),
    ).toBe('Alice left the room')
  })

  it('returns null for anything that is not a membership event', () => {
    expect(
      stateEventNotice({
        ...memberEvent({ content: { topic: 'hi' } }),
        type: 'm.room.topic',
        state_key: '',
      }),
    ).toBeNull()
    expect(stateEventNotice(memberEvent({ content: null }))).toBeNull()
  })
})
