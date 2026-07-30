import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import {
  currentRoomFromPath,
  hasNarrowingFilter,
  highlightSnippet,
  parseSearchTokens,
  queryTerms,
  serializeSearchTokens,
  toApiParams,
  withoutSearchParam,
  withSearchParam,
  type SearchQuery,
  type SearchTokenContext,
} from './search-tokens'
import type { RoomDto } from './stores/room-list'

const ACCOUNT_A = '6b53f7f0-0000-4000-8000-00000000000a'
const ACCOUNT_B = '6b53f7f0-0000-4000-8000-00000000000b'

function room(
  accountId: string,
  roomId: string,
  overrides: Partial<RoomDto> = {},
): RoomDto {
  return {
    account_id: accountId,
    account_user_id: accountId === ACCOUNT_A ? '@alice:hs' : '@bob:hs',
    room_id: roomId,
    last_activity_ts: 0,
    highlight_count: 0,
    notification_count: 0,
    ...overrides,
  }
}

const ctx: SearchTokenContext = {
  rooms: [
    room(ACCOUNT_A, '!ops:hs', { name: 'Ops Team' }),
    room(ACCOUNT_A, '!general:hs', { canonical_alias: '#general:hs' }),
    room(ACCOUNT_B, '!lounge:hs', { name: 'Lounge' }),
  ],
  accounts: [
    { account_id: ACCOUNT_A, user_id: '@alice:hs' },
    { account_id: ACCOUNT_B, user_id: '@bob:hs' },
  ],
  current: { accountId: ACCOUNT_A, roomId: '!ops:hs' },
}

const noRoomCtx: SearchTokenContext = { ...ctx, current: null }

describe('parseSearchTokens', () => {
  it('defaults the scope to the current room', () => {
    const { query, errors } = parseSearchTokens('deploy failed', ctx)
    expect(errors).toEqual([])
    expect(query.text).toBe('deploy failed')
    expect(query.scope).toEqual({
      kind: 'room',
      accountId: ACCOUNT_A,
      roomId: '!ops:hs',
    })
  })

  it('defaults to all accounts when no room is open', () => {
    const { query } = parseSearchTokens('deploy', noRoomCtx)
    expect(query.scope).toEqual({ kind: 'all' })
  })

  it('resolves room: by name, alias localpart, and id', () => {
    for (const target of ['ops', '"Ops Team"', '!ops:hs']) {
      const { query, errors } = parseSearchTokens(`room:${target} x`, ctx)
      expect(errors).toEqual([])
      expect(query.scope).toEqual({
        kind: 'room',
        accountId: ACCOUNT_A,
        roomId: '!ops:hs',
      })
    }
    const { query } = parseSearchTokens('room:general x', ctx)
    expect(query.scope).toEqual({
      kind: 'room',
      accountId: ACCOUNT_A,
      roomId: '!general:hs',
    })
  })

  it('narrows room: resolution to the account: filter', () => {
    const { query, errors } = parseSearchTokens(
      'room:lounge account:bob x',
      ctx,
    )
    expect(errors).toEqual([])
    expect(query.scope).toEqual({
      kind: 'room',
      accountId: ACCOUNT_B,
      roomId: '!lounge:hs',
    })
  })

  it('reports an unresolvable room as an error, not a request', () => {
    const { query, errors } = parseSearchTokens('room:nowhere x', ctx)
    expect(errors).toHaveLength(1)
    expect(errors[0]).toContain('nowhere')
    // Falls back to the current room rather than silently widening.
    expect(query.scope.kind).toBe('room')
  })

  it('maps room:* to the current account and account:* / all:true / * to everywhere', () => {
    expect(parseSearchTokens('room:* x', ctx).query.scope).toEqual({
      kind: 'account',
      accountId: ACCOUNT_A,
    })
    expect(parseSearchTokens('account:* x', ctx).query.scope).toEqual({
      kind: 'all',
    })
    expect(parseSearchTokens('all:true x', ctx).query.scope).toEqual({
      kind: 'all',
    })
    expect(parseSearchTokens('* x', ctx).query.scope).toEqual({
      kind: 'all',
    })
    expect(parseSearchTokens('"*" x', ctx).query).toMatchObject({
      text: '* x',
      scope: { kind: 'room', accountId: ACCOUNT_A, roomId: '!ops:hs' },
    })
  })

  it('resolves account: by uuid, mxid, and localpart', () => {
    for (const target of [ACCOUNT_B, '@bob:hs', 'bob', '@bob']) {
      const { query, errors } = parseSearchTokens(`account:${target} x`, ctx)
      expect(errors).toEqual([])
      expect(query.scope).toEqual({ kind: 'account', accountId: ACCOUNT_B })
    }
  })

  it('resolves literal ids with empty stores (a cold-loaded shared URL)', () => {
    const emptyCtx: SearchTokenContext = {
      rooms: [],
      accounts: [],
      current: null,
    }
    const { query, errors } = parseSearchTokens(
      `account:${ACCOUNT_A} room:!ops:hs x`,
      emptyCtx,
    )
    expect(errors).toEqual([])
    expect(query.scope).toEqual({
      kind: 'room',
      accountId: ACCOUNT_A,
      roomId: '!ops:hs',
    })
  })

  it('flags which fields tokens set explicitly', () => {
    const defaulted = parseSearchTokens('deploy', ctx)
    expect(defaulted.explicit).toEqual({
      scope: false,
      sender: false,
      from: false,
      to: false,
    })
    const explicit = parseSearchTokens('room:ops sender:bob deploy', ctx)
    expect(explicit.explicit.scope).toBe(true)
    expect(explicit.explicit.sender).toBe(true)
    expect(explicit.explicit.from).toBe(false)
  })

  it('rejects all:true combined with room: or account:', () => {
    const { errors } = parseSearchTokens('all:true room:ops x', ctx)
    expect(errors).toHaveLength(1)
    expect(errors[0]).toContain('all:true')
  })

  it('treats sender: and from: as synonyms and keeps quoted values whole', () => {
    expect(parseSearchTokens('sender:bob x', ctx).query.sender).toBe('bob')
    expect(parseSearchTokens('from:@bob:hs x', ctx).query.sender).toBe(
      '@bob:hs',
    )
    expect(parseSearchTokens('sender:"bob s" x', ctx).query.sender).toBe(
      'bob s',
    )
  })

  it('recognizes field tokens in any position', () => {
    // `matrix from:adam` and `from:adam matrix` must mean the same thing.
    const trailing = parseSearchTokens('deploy sender:bob', ctx)
    expect(trailing.query.sender).toBe('bob')
    expect(trailing.query.text).toBe('deploy')
    const sandwiched = parseSearchTokens('deploy from:bob failed', ctx)
    expect(sandwiched.query.sender).toBe('bob')
    expect(sandwiched.query.text).toBe('deploy failed')
  })

  it('treats everything after -- as literal text', () => {
    const { query } = parseSearchTokens('sender:bob -- room:layout bug', ctx)
    expect(query.sender).toBe('bob')
    expect(query.text).toBe('room:layout bug')
  })

  it('a quoted "--" is a search term, not the literal-mode terminator', () => {
    const { query } = parseSearchTokens('"--" deploy', ctx)
    expect(query.text).toBe('-- deploy')
  })

  it('a fully quoted field-shaped token is a phrase, not a field', () => {
    const { query, errors } = parseSearchTokens('"from:adam" matrix', ctx)
    expect(errors).toEqual([])
    expect(query.sender).toBeNull()
    expect(query.text).toBe('from:adam matrix')
  })

  it('reads field names case-insensitively', () => {
    const { query, errors } = parseSearchTokens('From:adam matrix', ctx)
    expect(errors).toEqual([])
    expect(query.sender).toBe('adam')
    expect(query.text).toBe('matrix')
  })

  it('rejects unknown field-like tokens with guidance, not a server 400', () => {
    // Tantivy reads `word:word` as a field query, so an unknown field
    // reaching `q` came back as a cryptic "Field does not exist" (live bug).
    const { query, errors } = parseSearchTokens('in:room deploy', ctx)
    expect(errors).toHaveLength(1)
    expect(errors[0]).toContain('unknown field "in:"')
    expect(errors[0]).toContain('--')
    expect(query.text).toBe('deploy')
  })

  it('lets URLs through as free text, quoted for the server', () => {
    const { query, errors } = parseSearchTokens(
      'find https://example.com docs',
      ctx,
    )
    expect(errors).toEqual([])
    expect(query.text).toBe('find https://example.com docs')
    expect(toApiParams(query).q).toBe('find "https://example.com" docs')
  })

  it('quotes literal colon tokens behind -- for the server', () => {
    const { query, errors } = parseSearchTokens(
      'room:ops -- from:adam matrix',
      ctx,
    )
    expect(errors).toEqual([])
    expect(toApiParams(query).q).toBe('"from:adam" matrix')
  })

  it('keeps quoted phrases intact in the free text', () => {
    const { query } = parseSearchTokens('"deploy failed" main', ctx)
    expect(query.text).toBe('"deploy failed" main')
  })

  describe('dates', () => {
    beforeEach(() => {
      vi.useFakeTimers()
      // A mid-month local noon, so day arithmetic cannot straddle months.
      vi.setSystemTime(new Date(2026, 6, 15, 12, 0, 0))
    })
    afterEach(() => vi.useRealTimers())

    it('parses ISO and US day forms into local day bounds', () => {
      const iso = parseSearchTokens('after:2026-07-01 x', ctx).query
      expect(iso.from).toBe(new Date(2026, 6, 1).getTime())
      const us = parseSearchTokens('before:7/1/26 x', ctx).query
      expect(us.to).toBe(new Date(2026, 6, 2).getTime() - 1)
    })

    it('parses today and yesterday', () => {
      const today = parseSearchTokens('after:today x', ctx).query
      expect(today.from).toBe(new Date(2026, 6, 15).getTime())
      const yesterday = parseSearchTokens('after:yesterday x', ctx).query
      expect(yesterday.from).toBe(new Date(2026, 6, 14).getTime())
    })

    it('expands date: to a whole day and date:A..B to a range', () => {
      const day = parseSearchTokens('date:2026-07-01 x', ctx).query
      expect(day.from).toBe(new Date(2026, 6, 1).getTime())
      expect(day.to).toBe(new Date(2026, 6, 2).getTime() - 1)
      const range = parseSearchTokens(
        'date:2026-07-01..2026-07-03 x',
        ctx,
      ).query
      expect(range.from).toBe(new Date(2026, 6, 1).getTime())
      expect(range.to).toBe(new Date(2026, 6, 4).getTime() - 1)
    })

    it('rejects impossible dates and reversed ranges', () => {
      expect(parseSearchTokens('after:2026-02-30 x', ctx).errors).toHaveLength(
        1,
      )
      const reversed = parseSearchTokens('date:2026-07-03..2026-07-01 x', ctx)
      expect(reversed.errors).toHaveLength(1)
      expect(reversed.query.from).toBeNull()
      expect(reversed.query.to).toBeNull()
    })
  })
})

describe('serializeSearchTokens', () => {
  it('round-trips through parse with ids, independent of the current room', () => {
    const { query } = parseSearchTokens(
      'room:general sender:bob after:2026-07-01 deploy "went wrong"',
      ctx,
    )
    const serialized = serializeSearchTokens(query)
    // Re-parse in a session with a *different* current room.
    const restored = parseSearchTokens(serialized, {
      ...ctx,
      current: { accountId: ACCOUNT_B, roomId: '!lounge:hs' },
    })
    expect(restored.errors).toEqual([])
    expect(restored.query).toEqual(query)
  })

  it('serializes scope explicitly even for a bare text query', () => {
    const { query } = parseSearchTokens('deploy', noRoomCtx)
    expect(serializeSearchTokens(query)).toBe('all:true deploy')
  })

  it('shields leading field-like text behind --', () => {
    const query: SearchQuery = {
      text: 'room:layout bug',
      scope: { kind: 'all' },
      sender: null,
      from: null,
      to: null,
    }
    const restored = parseSearchTokens(serializeSearchTokens(query), noRoomCtx)
    expect(restored.query.text).toBe('room:layout bug')
    expect(restored.query.scope).toEqual({ kind: 'all' })
  })
})

describe('toApiParams / hasNarrowingFilter', () => {
  it('maps each scope kind onto the endpoint parameters', () => {
    const base = { text: 'x', sender: null, from: null, to: null }
    expect(toApiParams({ ...base, scope: { kind: 'all' } })).toEqual({ q: 'x' })
    expect(
      toApiParams({
        ...base,
        scope: { kind: 'account', accountId: ACCOUNT_A },
      }),
    ).toEqual({ q: 'x', account_id: ACCOUNT_A })
    expect(
      toApiParams({
        ...base,
        scope: { kind: 'room', accountId: ACCOUNT_A, roomId: '!ops:hs' },
      }),
    ).toEqual({ q: 'x', account_id: ACCOUNT_A, room_id: '!ops:hs' })
  })

  it('omits q when the text is blank and carries sender and bounds', () => {
    const query: SearchQuery = {
      text: ' ',
      scope: { kind: 'all' },
      sender: 'bob',
      from: 1,
      to: 2,
    }
    expect(toApiParams(query)).toEqual({ sender: 'bob', from: 1, to: 2 })
    expect(hasNarrowingFilter(query)).toBe(true)
    expect(
      hasNarrowingFilter({
        text: 'x',
        scope: { kind: 'all' },
        sender: null,
        from: null,
        to: null,
      }),
    ).toBe(false)
  })
})

describe('URL helpers', () => {
  it('sets, preserves, and strips the search params', () => {
    const opened = withSearchParam('/a/rooms/!r?thread=%24t', 'room:ops x')
    expect(opened).toBe('/a/rooms/!r?thread=%24t&search=room%3Aops+x')
    expect(withoutSearchParam(`${opened}&ssort=newest`)).toBe(
      '/a/rooms/!r?thread=%24t',
    )
    expect(withoutSearchParam('/a/rooms/!r?search=x')).toBe('/a/rooms/!r')
  })

  it('reads the room out of a room path only', () => {
    expect(currentRoomFromPath(`/${ACCOUNT_A}/rooms/!ops:hs`)).toEqual({
      accountId: ACCOUNT_A,
      roomId: '!ops:hs',
    })
    expect(currentRoomFromPath('/settings')).toBeNull()
    expect(currentRoomFromPath('/')).toBeNull()
  })
})

describe('queryTerms / highlightSnippet', () => {
  it('extracts words and quoted phrases as match units', () => {
    expect(queryTerms('deploy "went wrong" main')).toEqual([
      'deploy',
      'went wrong',
      'main',
    ])
  })

  it('marks every term hit inside the window, case-insensitively', () => {
    const segments = highlightSnippet('The Deploy failed; redeploy later', [
      'deploy',
    ])
    expect(segments).toEqual([
      { text: 'The ', hit: false },
      { text: 'Deploy', hit: true },
      { text: ' failed; re', hit: false },
      { text: 'deploy', hit: true },
      { text: ' later', hit: false },
    ])
  })

  it('prefers the longest term at a position', () => {
    const segments = highlightSnippet('a redeployment happened', [
      'deploy',
      'deployment',
    ])
    expect(segments.filter((s) => s.hit).map((s) => s.text)).toEqual([
      'deployment',
    ])
  })

  it('windows a long body around the first hit with plain ellipses', () => {
    const body = `${'a'.repeat(200)} deploy ${'b'.repeat(200)}`
    const segments = highlightSnippet(body, ['deploy'])
    expect(segments[0]).toEqual({ text: '…', hit: false })
    expect(segments.at(-1)).toEqual({ text: '…', hit: false })
    expect(segments.some((s) => s.hit && s.text === 'deploy')).toBe(true)
  })

  it('collapses whitespace and truncates a no-hit body', () => {
    const flat = highlightSnippet('one\n\ntwo\tthree', [])
    expect(flat).toEqual([{ text: 'one two three', hit: false }])
    const long = highlightSnippet('x'.repeat(300), ['absent'])
    expect(long).toHaveLength(1)
    expect(long[0].text.endsWith('…')).toBe(true)
    expect(long[0].text.length).toBe(141)
  })

  it('returns nothing for an empty body', () => {
    expect(highlightSnippet('', ['x'])).toEqual([])
  })
})
