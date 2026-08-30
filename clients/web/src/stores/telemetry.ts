import type { CacheArea, CacheStore } from './cache-store'

/**
 * Durable performance telemetry (follow-up to the room-open instrumentation).
 *
 * The readout it feeds exists because iOS Safari has no on-device console, so
 * numbers are read off a screen recording (ADR 0077). That works, but only
 * when someone is *watching*: the slow load this was built to characterise has
 * never once happened while a screen recording was running. Persisting the
 * summaries removes that requirement — the next bad load is captured whether
 * or not anyone noticed it.
 */

const AREA: CacheArea = 'meta'
const KEY = 'telemetry.v1'

/** Bumped when the record below changes shape incompatibly. */
const RECORD_VERSION = 1

/** Sessions retained. Each is a handful of lines; this is days of use. */
const SESSION_LIMIT = 20

/** Entries kept per session, so one pathological session cannot grow forever. */
const ENTRY_LIMIT = 200

/**
 * The marks that reach disk, as an explicit allow-list of *names*.
 *
 * Every one of these is a reduced summary: numbers, plus a few enumerated
 * strings (`phase`, `via`, `nav`, `pending`) and a route shape whose ids are
 * already collapsed by `shortRoute`. None carries an identifier.
 *
 * The raw mark stream very much does — `room-page:initial-load-effect` carries
 * `accountId` and `roomId`, and both `room-list:navigate-to-room` and
 * `room-row:hydrate-preview` carry a room key. Persisting those would build a
 * durable record of which rooms were opened and when, which is a new category
 * of data at rest and exactly what ADR 0085's Privacy section exists to
 * refuse. Hence a list of what may be written rather than a list of what may
 * not.
 */
export const PERSISTED_MARKS: readonly string[] = [
  'boot:room-list',
  'boot:room-open',
  'boot:room-open:req',
  'transition:back',
]

export interface TelemetryEntry {
  /** Milliseconds since this document's navigation start. */
  at: number
  name: string
  detail: Record<string, string | number | boolean | null>
}

export interface TelemetrySession {
  /** Distinguishes documents; not correlated with anything server-side. */
  id: string
  /** Wall clock at session start, so a reader can date a capture. */
  startedAt: number
  entries: TelemetryEntry[]
}

interface TelemetryRecord {
  version: number
  sessions: TelemetrySession[]
}

export interface TelemetryStore {
  /** Append one summary mark, if it is on the allow-list. Fire-and-forget. */
  record(name: string, at: number, detail: unknown): void
  /** Every retained session, oldest first. */
  read(): Promise<TelemetrySession[]>
  /** Drop everything this store has written. */
  clear(): Promise<void>
}

/**
 * Values that must never reach disk even from an allow-listed mark.
 *
 * A name allow-list stops a *mark* leaking; it does not stop a field being
 * added to one of these four later. Matrix identifiers and UUIDs have distinct
 * shapes, so they can be rejected on sight — the same "mechanism, not a
 * promise" reasoning behind `room-list-cache.ts`'s projection.
 */
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i

function identifierish(value: string): boolean {
  return /^[!@#$+]/.test(value) || UUID.test(value)
}

/** Keep numbers, booleans and null; keep strings only if they name nothing. */
export function scrub(
  detail: unknown,
): Record<string, string | number | boolean | null> {
  if (typeof detail !== 'object' || detail === null) {
    return {}
  }
  const safe: Record<string, string | number | boolean | null> = {}
  for (const [key, value] of Object.entries(
    detail as Record<string, unknown>,
  )) {
    if (
      value === null ||
      typeof value === 'number' ||
      typeof value === 'boolean'
    ) {
      safe[key] = value
    } else if (typeof value === 'string' && !identifierish(value)) {
      safe[key] = value
    }
    // Anything else — objects, arrays, an identifier-shaped string — is
    // dropped rather than coerced. A field nobody vetted is not written.
  }
  return safe
}

export interface TelemetryStoreOptions {
  cache: CacheStore
  /** The user's setting, read per call so a toggle takes effect at once. */
  enabled: () => boolean
  /** Injected for tests; `crypto.randomUUID` in production. */
  sessionId?: () => string
  now?: () => number
}

export function createTelemetryStore({
  cache,
  enabled,
  sessionId = () => Math.random().toString(36).slice(2, 10),
  now = () => Date.now(),
}: TelemetryStoreOptions): TelemetryStore {
  const session: TelemetrySession = {
    id: sessionId(),
    startedAt: now(),
    entries: [],
  }
  // Writes are coalesced: a room open emits a summary plus up to four request
  // lines within a frame, and one IndexedDB transaction per line would put
  // storage work on the path being measured.
  let flushing = false
  /**
   * The cache generation these buffered entries belong to.
   *
   * Captured when an entry is *recorded*, not when it is written. A wipe is a
   * privacy barrier, and the entries buffered before it were recorded before
   * the user asked for deletion — reading the generation inside the flush
   * would sample it after the wipe and happily write them back.
   */
  let generation = cache.generation

  /** Discard the buffer if a wipe has happened since it was filled. */
  function wiped(): boolean {
    if (cache.generation === generation) {
      return false
    }
    session.entries = []
    generation = cache.generation
    return true
  }

  function schedule(): void {
    if (flushing) {
      return
    }
    flushing = true
    setTimeout(() => {
      flushing = false
      void flush()
    }, 0)
  }

  async function flush(): Promise<void> {
    if (wiped() || !enabled() || session.entries.length === 0) {
      return
    }
    const record = await load()
    const sessions = record.sessions.filter((each) => each.id !== session.id)
    sessions.push({ ...session, entries: [...session.entries] })
    // Re-checked after the read, not only before it: a wipe that begins inside
    // that await must still win.
    if (wiped()) {
      return
    }
    await cache.write<TelemetryRecord>(AREA, KEY, {
      version: RECORD_VERSION,
      sessions: sessions.slice(-SESSION_LIMIT),
    })
  }

  async function load(): Promise<TelemetryRecord> {
    const record = await cache.read<TelemetryRecord>(AREA, KEY)
    if (
      record === undefined ||
      record === null ||
      typeof record !== 'object' ||
      record.version !== RECORD_VERSION ||
      !Array.isArray(record.sessions)
    ) {
      return { version: RECORD_VERSION, sessions: [] }
    }
    return record
  }

  return {
    record(name, at, detail) {
      if (!enabled() || !PERSISTED_MARKS.includes(name)) {
        return
      }
      wiped()
      session.entries.push({
        at: Math.round(at),
        name,
        detail: scrub(detail),
      })
      if (session.entries.length > ENTRY_LIMIT) {
        session.entries = session.entries.slice(-ENTRY_LIMIT)
      }
      schedule()
    },

    async read() {
      return (await load()).sessions
    },

    async clear() {
      session.entries = []
      await cache.drop(AREA, KEY)
    },
  }
}

/**
 * Render sessions as the plain text a bug report can carry.
 *
 * Deliberately the same shape as the on-screen overlay, so a pasted capture
 * and a screenshotted one read identically and `docs/web-slow-link-measurement.md`
 * describes both.
 */
export function formatTelemetry(sessions: readonly TelemetrySession[]): string {
  if (sessions.length === 0) {
    return 'No telemetry recorded.'
  }
  return sessions
    .map((session) => {
      const header = `# session ${session.id} — ${new Date(session.startedAt).toISOString()}`
      const lines = session.entries.map((entry) => {
        const detail = Object.entries(entry.detail)
          .map(([key, value]) => `${key}=${String(value)}`)
          .join(' ')
        return `${entry.at} ${entry.name}${detail === '' ? '' : ` ${detail}`}`
      })
      return [header, ...lines].join('\n')
    })
    .join('\n\n')
}
