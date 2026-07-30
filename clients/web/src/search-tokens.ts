import type { operations } from './api/schema'
import { isoCalendarDay, parseCalendarDay } from './calendar-day'
import { resolveRoomTarget } from './stores/room-command'
import type { RoomDto } from './stores/room-list'

/**
 * The search query model (ADR 0066): one shape that is simultaneously the
 * token-grammar parse result, the chip model the overlay renders, the URL
 * serialization (`?search=` carries the token string), and the source of the
 * `GET /v1/search` parameters. Keeping all four surfaces on this one type is
 * what stops them drifting.
 *
 * The grammar is a deliberate subset of the TUI's
 * (`clients/tui/src/search.rs`): `room:`, `account:`, `all:true`, `sender:`
 * (synonym `from:`), `after:`, `before:`, `date:` (day or `A..B`), quoted
 * values, and the `--` free-text terminator. `received:`, `limit:` and the
 * TUI's human-date phrases are dropped; see the ADR for why.
 */

export type SearchScope =
  | { kind: 'room'; accountId: string; roomId: string }
  | { kind: 'account'; accountId: string }
  | { kind: 'all' }

export interface SearchQuery {
  /** Free-text terms, quoted phrases preserved — the server's `q`. */
  text: string
  scope: SearchScope
  /** Case-insensitive MXID substring, or null. */
  sender: string | null
  /** Inclusive lower bound on `origin_ts`, Unix ms, or null. */
  from: number | null
  /** Inclusive upper bound on `origin_ts`, Unix ms, or null. */
  to: number | null
}

/** The slice of an account the parser needs to resolve `account:` tokens. */
export interface SearchAccount {
  account_id: string
  user_id: string
}

export interface SearchTokenContext {
  rooms: readonly RoomDto[]
  accounts: readonly SearchAccount[]
  /** The room the user is looking at, if any — the default scope. */
  current: { accountId: string; roomId: string } | null
}

export type SearchApiParams = NonNullable<
  operations['search']['parameters']['query']
>

/** Case-insensitive: `From:adam` means the field, not the word `From:adam`. */
const FIELD_PATTERN = /^([A-Za-z]+):(.*)$/
const FIELDS = new Set([
  'room',
  'account',
  'all',
  'sender',
  'from',
  'after',
  'before',
  'date',
])

/**
 * A token plus whether it *opened* with a quote — `"--"` or `"from:adam"` is
 * the user quoting a literal term, while `sender:"bob s"` quotes only the
 * value and still parses as a field.
 */
interface RawToken {
  text: string
  quotedStart: boolean
}

/**
 * Split on whitespace, honoring double quotes: `sender:"bob s"` is one token
 * whose value contains the space. Quotes group; they are not part of the
 * value. An unterminated quote runs to the end of input rather than erroring —
 * the parser runs on every submit, and half-typed input is the common case.
 */
function tokenizeRaw(input: string): RawToken[] {
  const tokens: RawToken[] = []
  let current = ''
  let inQuote = false
  let sawAny = false
  let quotedStart = false
  for (const char of input) {
    if (char === '"') {
      inQuote = !inQuote
      if (!sawAny) {
        quotedStart = true
      }
      sawAny = true
      continue
    }
    if (!inQuote && /\s/.test(char)) {
      if (sawAny) {
        tokens.push({ text: current, quotedStart })
        current = ''
        sawAny = false
        quotedStart = false
      }
      continue
    }
    current += char
    sawAny = true
  }
  if (sawAny) {
    tokens.push({ text: current, quotedStart })
  }
  return tokens
}

function tokenize(input: string): string[] {
  return tokenizeRaw(input).map((token) => token.text)
}

/** Re-quote a value whose spaces the tokenizer would otherwise split. */
function quoteIfNeeded(value: string): string {
  return /\s/.test(value) ? `"${value}"` : value
}

const UUID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i

function resolveAccount(
  accounts: readonly SearchAccount[],
  target: string,
): SearchAccount | null {
  const lower = target.toLowerCase()
  const known = accounts.find(
    (account) =>
      account.account_id === target ||
      account.user_id.toLowerCase() === lower ||
      // `@adam:hs` → `adam`; allow `account:adam` and `account:@adam`.
      account.user_id.toLowerCase().startsWith(`${lower.replace(/^@?/, '@')}:`),
  )
  if (known !== undefined) {
    return known
  }
  // A literal account uuid resolves even before the accounts store has
  // loaded — serialized URLs carry uuids, and a cold-loaded shared link must
  // not error on an empty store. A wrong uuid just finds nothing server-side.
  return UUID_PATTERN.test(target) ? { account_id: target, user_id: '' } : null
}

/**
 * Parse a token string into a query. Never throws: unresolvable or
 * conflicting tokens land in `errors` (rendered as chip errors, not sent to
 * the server) and the query keeps its best reading of the rest. Field tokens
 * are recognized anywhere in the string — order carries no meaning (a
 * departure from the TUI, whose fields must precede the text); everything
 * after a literal `--` is query text verbatim.
 */
export interface ParsedSearch {
  query: SearchQuery
  errors: string[]
  /**
   * Which query fields the input set with an actual token, as opposed to a
   * default. The overlay merges typed tokens over its chip state, and a
   * defaulted scope must not clobber a chip the user has changed by hand.
   */
  explicit: { scope: boolean; sender: boolean; from: boolean; to: boolean }
}

export function parseSearchTokens(
  input: string,
  ctx: SearchTokenContext,
): ParsedSearch {
  const errors: string[] = []
  const textParts: string[] = []
  let all = false
  let accountTarget: string | null = null
  let roomTarget: string | null = null
  let sender: string | null = null
  let from: number | null = null
  let to: number | null = null

  // Field tokens are recognized anywhere, not only before the free text —
  // `matrix from:adam` and `from:adam matrix` must mean the same thing
  // (nobody remembers an ordering rule). `--` still makes everything after
  // it literal, which is how a message *about* `from:adam` stays findable.
  let literal = false
  for (const { text: token, quotedStart } of tokenizeRaw(input)) {
    // A token the user opened with a quote is literal text: `"--"` searches
    // for the string `--` instead of terminating field parsing, and
    // `"from:adam"` is a phrase, not a field.
    if (!literal && !quotedStart && token === '--') {
      literal = true
      continue
    }
    // A bare `*` is the compact spelling for searching every account. It is
    // deliberately recognized only as an unquoted token before `--`, so a
    // user can still search for an asterisk literally.
    if (!literal && !quotedStart && token === '*') {
      all = true
      continue
    }
    const match = literal || quotedStart ? null : FIELD_PATTERN.exec(token)
    const field = match?.[1].toLowerCase()
    if (match === null || field === undefined || match[2] === '') {
      textParts.push(quoteIfNeeded(token))
      continue
    }
    if (!FIELDS.has(field)) {
      // `word://…` is a URL someone is searching for, not a field attempt.
      // Anything else shaped like a field is almost certainly a typo or a
      // guessed field name (`in:room`), and the server would answer it with
      // Tantivy's cryptic `Field does not exist` — its query syntax also
      // reads `word:word` as a field. Say what the fields are instead.
      if (match[2].startsWith('/')) {
        textParts.push(quoteIfNeeded(token))
        continue
      }
      errors.push(
        `unknown field "${match[1]}:" — fields are room:, account:, ` +
          `sender:/from:, after:, before:, date:, all: ` +
          `(prefix with -- to search for the literal text)`,
      )
      continue
    }
    const value = match[2]
    switch (field) {
      case 'all':
        if (value.toLowerCase() === 'true') {
          all = true
        } else {
          errors.push(`all: takes only "true", got "${value}"`)
        }
        break
      case 'room':
        roomTarget = value
        break
      case 'account':
        accountTarget = value
        break
      case 'sender':
      case 'from':
        sender = value
        break
      case 'after': {
        const day = parseCalendarDay(value)
        if (day === null) {
          errors.push(`unrecognized date "${value}"`)
        } else {
          from = day.start
        }
        break
      }
      case 'before': {
        const day = parseCalendarDay(value)
        if (day === null) {
          errors.push(`unrecognized date "${value}"`)
        } else {
          to = day.end
        }
        break
      }
      case 'date': {
        const range = value.split('..')
        const first = parseCalendarDay(range[0])
        const last =
          range.length > 1 ? parseCalendarDay(range[range.length - 1]) : first
        if (first === null || last === null || range.length > 2) {
          errors.push(`unrecognized date "${value}"`)
        } else {
          from = first.start
          to = last.end
        }
        break
      }
    }
  }

  if (from !== null && to !== null && from > to) {
    errors.push('date range start is after its end')
    from = null
    to = null
  }

  // Scope resolution, with the TUI's cross-validation: `all:true` cannot be
  // narrowed back down by `room:`/`account:`.
  if (all && (roomTarget !== null || accountTarget !== null)) {
    errors.push('all:true cannot combine with room: or account:')
    roomTarget = null
    accountTarget = null
  }

  let scope: SearchScope
  const account =
    accountTarget === null || accountTarget === '*'
      ? null
      : resolveAccount(ctx.accounts, accountTarget)
  if (accountTarget !== null && accountTarget !== '*' && account === null) {
    errors.push(`no account matches "${accountTarget}"`)
  }

  if (all || accountTarget === '*') {
    scope = { kind: 'all' }
  } else if (roomTarget === '*') {
    // All rooms of the named account, else of the current one, else truly all.
    const accountId = account?.account_id ?? ctx.current?.accountId
    scope =
      accountId === undefined ? { kind: 'all' } : { kind: 'account', accountId }
  } else if (roomTarget !== null) {
    const candidates =
      account === null
        ? ctx.rooms
        : ctx.rooms.filter((room) => room.account_id === account.account_id)
    const resolution = resolveRoomTarget(candidates, roomTarget)
    const literalAccountId = account?.account_id ?? ctx.current?.accountId
    if (resolution.kind === 'match') {
      scope = {
        kind: 'room',
        accountId: resolution.room.account_id,
        roomId: resolution.room.room_id,
      }
    } else if (
      resolution.kind === 'missing' &&
      roomTarget.startsWith('!') &&
      roomTarget.includes(':') &&
      literalAccountId !== undefined
    ) {
      // A literal room id resolves without the rooms store, so a cold-loaded
      // shared URL (always id-serialized) works before the list arrives.
      scope = { kind: 'room', accountId: literalAccountId, roomId: roomTarget }
    } else {
      errors.push(
        resolution.kind === 'ambiguous'
          ? `"${roomTarget}" matches several rooms`
          : `no room matches "${roomTarget}"`,
      )
      scope =
        account !== null
          ? { kind: 'account', accountId: account.account_id }
          : ctx.current !== null
            ? { kind: 'room', ...ctx.current }
            : { kind: 'all' }
    }
  } else if (account !== null) {
    scope = { kind: 'account', accountId: account.account_id }
  } else if (accountTarget !== null) {
    // An unresolvable `account:` already errored; fall back rather than
    // silently searching everything under a filter the user thought was set.
    scope =
      ctx.current !== null ? { kind: 'room', ...ctx.current } : { kind: 'all' }
  } else {
    // No scope token: the point of invocation decides (ADR 0066) — the open
    // room when there is one, everywhere otherwise.
    scope =
      ctx.current !== null ? { kind: 'room', ...ctx.current } : { kind: 'all' }
  }

  return {
    query: { text: textParts.join(' '), scope, sender, from, to },
    errors,
    explicit: {
      scope: all || accountTarget !== null || roomTarget !== null,
      sender: sender !== null,
      from: from !== null,
      to: to !== null,
    },
  }
}

/**
 * A query back to its token string — the `?search=` value. Ids, not names:
 * a shared URL must resolve identically in a session with different rooms
 * loaded, so the human-readable spellings are parse-time sugar only. Scope is
 * always explicit for the same reason (a restored link must not re-default to
 * whatever room the recipient has open).
 */
export function serializeSearchTokens(query: SearchQuery): string {
  const tokens: string[] = []
  switch (query.scope.kind) {
    case 'all':
      tokens.push('all:true')
      break
    case 'account':
      tokens.push(`account:${query.scope.accountId}`)
      break
    case 'room':
      tokens.push(
        `account:${query.scope.accountId}`,
        `room:${quoteIfNeeded(query.scope.roomId)}`,
      )
      break
  }
  if (query.sender !== null && query.sender !== '') {
    tokens.push(`sender:${quoteIfNeeded(query.sender)}`)
  }
  if (query.from !== null) {
    tokens.push(`after:${isoCalendarDay(query.from)}`)
  }
  if (query.to !== null) {
    tokens.push(`before:${isoCalendarDay(query.to)}`)
  }
  const text = query.text.trim()
  if (text !== '') {
    // Fields parse anywhere, so *any* text token that reads as a field would
    // be eaten on re-parse; `--` makes the whole tail literal.
    const fieldLike = tokenize(text).some((token) => {
      const match = FIELD_PATTERN.exec(token)
      return (
        match !== null && FIELDS.has(match[1].toLowerCase()) && match[2] !== ''
      )
    })
    if (fieldLike || text.startsWith('--')) {
      tokens.push('--')
    }
    tokens.push(text)
  }
  return tokens.join(' ')
}

/** The `GET /v1/search` parameters for a query (no paging — the store's job). */
export function toApiParams(query: SearchQuery): SearchApiParams {
  const params: SearchApiParams = {}
  const text = query.text.trim()
  if (text !== '') {
    // Tantivy's own query syntax reads a bare `word:word` as a field query
    // and 400s on unknown fields, so a colon-bearing term — a URL, a literal
    // `from:adam` behind `--` — must ride inside quotes, where the colon is
    // just a token separator (verified against the live index).
    params.q = tokenize(text)
      .map((token) =>
        token.includes(':') ? `"${token}"` : quoteIfNeeded(token),
      )
      .join(' ')
  }
  if (query.scope.kind === 'room') {
    params.account_id = query.scope.accountId
    params.room_id = query.scope.roomId
  } else if (query.scope.kind === 'account') {
    params.account_id = query.scope.accountId
  }
  if (query.sender !== null && query.sender !== '') {
    params.sender = query.sender
  }
  if (query.from !== null) {
    params.from = query.from
  }
  if (query.to !== null) {
    params.to = query.to
  }
  return params
}

/**
 * Whether the query narrows beyond free text. The server rejects an empty `q`
 * with no narrowing filter as a 400; the overlay uses this to refuse the
 * round trip up front.
 */
export function hasNarrowingFilter(query: SearchQuery): boolean {
  return (
    query.scope.kind !== 'all' ||
    (query.sender !== null && query.sender !== '') ||
    query.from !== null ||
    query.to !== null
  )
}

// ---------------------------------------------------------------------------
// The `?search=` URL contract (ADR 0066): the param's presence opens the
// overlay, its value is the token string, and `ssort` carries a non-default
// result sort. Both ride the *current* route so opening search never unmounts
// the page beneath it.

/** `url` with `search` set to `tokens`, other params preserved. */
export function withSearchParam(url: string, tokens: string): string {
  const [base, queryString = ''] = url.split('?', 2)
  const params = new URLSearchParams(queryString)
  params.set('search', tokens)
  return `${base}?${params.toString()}`
}

/** `url` with the search overlay's params stripped — the "closed" URL. */
export function withoutSearchParam(url: string): string {
  const [base, queryString = ''] = url.split('?', 2)
  const params = new URLSearchParams(queryString)
  params.delete('search')
  params.delete('ssort')
  const rest = params.toString()
  return rest === '' ? base : `${base}?${rest}`
}

/**
 * The room a path is looking at, for the default search scope — the
 * `/:accountId/rooms/:roomId` route shape from `app.tsx`, or null on any
 * other page.
 */
export function currentRoomFromPath(
  path: string,
): { accountId: string; roomId: string } | null {
  const match = /^\/([^/]+)\/rooms\/([^/]+)$/.exec(path)
  if (match === null) {
    return null
  }
  return {
    accountId: decodeURIComponent(match[1]),
    roomId: decodeURIComponent(match[2]),
  }
}

// ---------------------------------------------------------------------------
// Snippets. The server sends no highlights or context (ADR 0047 made that a
// client concern), so the hit list builds its own: a window around the first
// term match in the *plain-text* body, with every match inside the window
// marked. Never fed `formatted_body` — slicing HTML at arbitrary offsets is
// an injection surface (ADR 0066).

export interface SnippetSegment {
  text: string
  hit: boolean
}

/**
 * The match units of a free-text query: bare words and quoted phrases, in
 * writing order. These drive highlighting only — the server has its own
 * parse of `q` for ranking.
 */
export function queryTerms(text: string): string[] {
  return tokenize(text).filter((term) => term !== '' && term !== '--')
}

const SNIPPET_RADIUS = 60
/** Cap for a no-hit (or no-term) snippet, roughly two rendered lines. */
const SNIPPET_PLAIN_LIMIT = 140

/**
 * Segment a body for a result row: a ±`SNIPPET_RADIUS` window around the
 * first term hit, ellipsized at clipped edges, `hit: true` on every match
 * inside the window (longest term wins at a position). Case-insensitive.
 * With no terms or no match, the head of the body, truncated.
 */
export function highlightSnippet(
  body: string,
  terms: string[],
): SnippetSegment[] {
  const flat = body.replace(/\s+/g, ' ').trim()
  const lower = flat.toLowerCase()
  // Longest first, so `deployment` is not eaten by a shorter `deploy` at the
  // same position.
  const needles = [...new Set(terms.map((term) => term.toLowerCase()))]
    .filter((term) => term !== '')
    .sort((a, b) => b.length - a.length)

  let firstHit = -1
  let firstLength = 0
  for (const needle of needles) {
    const at = lower.indexOf(needle)
    if (at !== -1 && (firstHit === -1 || at < firstHit)) {
      firstHit = at
      firstLength = needle.length
    }
  }

  if (firstHit === -1) {
    if (flat.length <= SNIPPET_PLAIN_LIMIT) {
      return flat === '' ? [] : [{ text: flat, hit: false }]
    }
    return [{ text: `${flat.slice(0, SNIPPET_PLAIN_LIMIT)}…`, hit: false }]
  }

  const start = Math.max(0, firstHit - SNIPPET_RADIUS)
  const end = Math.min(flat.length, firstHit + firstLength + SNIPPET_RADIUS)
  const segments: SnippetSegment[] = []
  let plainFrom = start

  const pushPlain = (upTo: number, text?: string) => {
    const value = text ?? flat.slice(plainFrom, upTo)
    if (value !== '') {
      segments.push({ text: value, hit: false })
    }
  }

  let cursor = start
  while (cursor < end) {
    const needle = needles.find((candidate) =>
      lower.startsWith(candidate, cursor),
    )
    if (needle === undefined) {
      cursor += 1
      continue
    }
    pushPlain(cursor)
    segments.push({
      text: flat.slice(cursor, cursor + needle.length),
      hit: true,
    })
    cursor += needle.length
    plainFrom = cursor
  }
  pushPlain(end)

  // Ellipses ride their own plain segments: gluing one onto an edge segment
  // would render it inside a <mark> when the window starts or ends on a hit.
  if (start > 0) {
    segments.unshift({ text: '…', hit: false })
  }
  if (end < flat.length) {
    segments.push({ text: '…', hit: false })
  }
  return segments
}
