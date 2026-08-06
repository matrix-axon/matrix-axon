import { describe, expect, it } from 'vitest'
import type { TimelineEvent } from '../stores/timeline'
import {
  GALLERY_MAX,
  GALLERY_WINDOW_MS,
  groupMediaRuns,
  rowLeadEvent,
  rowTs,
  runPosition,
  type TimelineRow,
} from './group-media-runs'

const DAY = 24 * 60 * 60 * 1000
/** Noon, so a `+1 day` in a test cannot accidentally straddle midnight. */
const BASE = new Date(2026, 6, 26, 12, 0, 0).getTime()

function image(
  id: string,
  overrides: Partial<TimelineEvent> = {},
  ts = BASE,
): TimelineEvent {
  return {
    account_id: '11111111-1111-4111-8111-111111111111',
    event_id: id,
    room_id: '!r:hs',
    sender: '@alice:hs',
    origin_ts: ts,
    arrival_order: ts,
    type: 'm.room.message',
    body: `${id}.png`,
    content: {
      msgtype: 'm.image',
      body: `${id}.png`,
      url: `mxc://hs/${id}`,
      info: { mimetype: 'image/png', size: 10, w: 80, h: 60 },
    },
    redacted: false,
    edited: false,
    edit_count: 0,
    state_key: null,
    reactions: null,
    relates_to: null,
    ...overrides,
  } as unknown as TimelineEvent
}

function text(id: string, ts = BASE): TimelineEvent {
  return image(
    id,
    {
      body: 'hi',
      content: { msgtype: 'm.text', body: 'hi' },
    } as unknown as Partial<TimelineEvent>,
    ts,
  )
}

/** A compact shape for asserting layout: `E` for a row, `[a,b]` for a gallery. */
function shape(rows: TimelineRow[]): string {
  return rows
    .map((row) =>
      row.kind === 'event'
        ? row.event.event_id
        : `[${row.events.map((e) => e.event_id).join(',')}]`,
    )
    .join(' ')
}

describe('groupMediaRuns', () => {
  it('leaves a timeline with no media alone', () => {
    expect(shape(groupMediaRuns([text('$a'), text('$b')]))).toBe('$a $b')
  })

  it('groups adjacent images from one sender', () => {
    const rows = groupMediaRuns([image('$1'), image('$2'), image('$3')])
    expect(shape(rows)).toBe('[$1,$2,$3]')
  })

  it('leaves a lone image as an ordinary row', () => {
    expect(shape(groupMediaRuns([text('$a'), image('$1'), text('$b')]))).toBe(
      '$a $1 $b',
    )
  })

  it('flushes a run of one back to an ordinary row', () => {
    // Below `min`, so it must not become a one-cell gallery.
    expect(shape(groupMediaRuns([image('$1'), text('$a'), image('$2')]))).toBe(
      '$1 $a $2',
    )
  })

  describe('what breaks a run', () => {
    it('media that is not a picture', () => {
      // The grid draws every cell as a square image. A file or a voice memo
      // has a download card the cell cannot render, so it belongs in an
      // ordinary row — and it separates the images either side of it.
      const file = image('$f', {
        body: 'notes.pdf',
        content: {
          msgtype: 'm.file',
          body: 'notes.pdf',
          url: 'mxc://hs/f',
          info: { mimetype: 'application/pdf', size: 10 },
        },
      } as unknown as Partial<TimelineEvent>)
      const rows = groupMediaRuns([
        image('$1'),
        image('$2'),
        file,
        image('$3'),
        image('$4'),
      ])
      expect(shape(rows)).toBe('[$1,$2] $f [$3,$4]')
    })

    it('an intervening message of any kind', () => {
      const rows = groupMediaRuns([
        image('$1'),
        image('$2'),
        text('$a'),
        image('$3'),
        image('$4'),
      ])
      expect(shape(rows)).toBe('[$1,$2] $a [$3,$4]')
    })

    it('a different sender', () => {
      const rows = groupMediaRuns([
        image('$1'),
        image('$2'),
        image('$3', { sender: '@bob:hs' }),
        image('$4', { sender: '@bob:hs' }),
      ])
      expect(shape(rows)).toBe('[$1,$2] [$3,$4]')
    })

    it('a gap wider than the window', () => {
      const rows = groupMediaRuns([
        image('$1', {}, BASE),
        image('$2', {}, BASE + GALLERY_WINDOW_MS),
        image('$3', {}, BASE + GALLERY_WINDOW_MS * 2 + 1),
        image('$4', {}, BASE + GALLERY_WINDOW_MS * 2 + 2),
      ])
      expect(shape(rows)).toBe('[$1,$2] [$3,$4]')
    })

    it('a wide gap in a newest-first list', () => {
      // Found by e2e: a signed `event - previous <= window` passes for *any*
      // negative delta, so a descending list grouped images hours apart. The
      // comparison is absolute so ordering is not silently assumed.
      const rows = groupMediaRuns([
        image('$3', {}, BASE),
        image('$2', {}, BASE - 30 * 60_000),
        image('$1', {}, BASE - 60 * 60_000),
      ])
      expect(shape(rows)).toBe('$3 $2 $1')
    })

    it('a day boundary, even inside the window', () => {
      // Otherwise a gallery could span a day separator, which the timeline
      // renders between rows.
      const lateNight = new Date(2026, 6, 26, 23, 59, 50).getTime()
      const rows = groupMediaRuns([
        image('$1', {}, lateNight),
        image('$2', {}, lateNight + 20_000),
      ])
      expect(shape(rows)).toBe('$1 $2')
    })

    it('a redaction', () => {
      const rows = groupMediaRuns([
        image('$1'),
        image('$2', { redacted: true }),
        image('$3'),
      ])
      expect(shape(rows)).toBe('$1 $2 $3')
    })

    it('a state event', () => {
      const rows = groupMediaRuns([
        image('$1'),
        image('$2', { state_key: '' }),
        image('$3'),
      ])
      expect(shape(rows)).toBe('$1 $2 $3')
    })

    it('a reaction', () => {
      // Cost of the split is accepted in ADR 0081: reacting to a middle image
      // turns one row into three, live.
      const rows = groupMediaRuns([
        image('$1'),
        image('$2'),
        image('$3', {
          // A per-emoji tally keyed by emoji, the shape the API returns.
          reactions: { '👍': { count: 1, me: false, senders: ['@bob:hs'] } },
        } as unknown as Partial<TimelineEvent>),
        image('$4'),
        image('$5'),
      ])
      expect(shape(rows)).toBe('[$1,$2] $3 [$4,$5]')
    })

    it('a reply', () => {
      const rows = groupMediaRuns([
        image('$1'),
        image('$2', {
          relates_to: { 'm.in_reply_to': { event_id: '$x' } },
        } as unknown as Partial<TimelineEvent>),
        image('$3'),
      ])
      expect(shape(rows)).toBe('$1 $2 $3')
    })

    it('a failed send, which needs its retry affordance', () => {
      const rows = groupMediaRuns([
        image('$1'),
        image('$2', {
          localEcho: { status: 'failed', body: 'x', options: {} },
        } as unknown as Partial<TimelineEvent>),
        image('$3'),
      ])
      expect(shape(rows)).toBe('$1 $2 $3')
    })

    it('the caller’s own predicate', () => {
      const rows = groupMediaRuns([image('$1'), image('$2'), image('$3')], {
        breaks: (event) => event.event_id === '$2',
      })
      expect(shape(rows)).toBe('$1 $2 $3')
    })
  })

  describe('what does not break a run', () => {
    it('a still-uploading echo groups with its neighbours', () => {
      // A multi-image send pushes all echoes at once; they must appear as one
      // gallery immediately, not as N rows that collapse later.
      const pending = {
        localEcho: {
          status: 'pending',
          body: 'cat.png',
          options: {},
          media: {
            file: new File(['x'], 'cat.png', { type: 'image/png' }),
            kind: 'image',
            previewUrl: 'blob:preview',
          },
        },
        content: null,
      } as unknown as Partial<TimelineEvent>
      const rows = groupMediaRuns([
        image('$1', pending),
        image('$2', pending),
        image('$3', pending),
      ])
      expect(shape(rows)).toBe('[$1,$2,$3]')
    })

    it('an edited caption keeps its image in the gallery', () => {
      // Reported from real use: correcting one caption dropped the whole
      // gallery back to five separate rows. An edit changes content the grid
      // already shows beneath the cells, not structure — and `origin_ts` is
      // untouched by an edit, so the run's timing does not move either.
      const rows = groupMediaRuns([
        image('$1'),
        image('$2', {
          edited: true,
          edit_count: 1,
          latest_edit_ts: BASE + 9e6,
        }),
        image('$3'),
      ])
      expect(shape(rows)).toBe('[$1,$2,$3]')
    })

    it('groups on the original posting time, not the edit time', () => {
      // `latest_edit_ts` can be hours later; grouping must not see it.
      const rows = groupMediaRuns([
        image('$1', {}, BASE),
        image(
          '$2',
          { edited: true, latest_edit_ts: BASE + 6 * 60 * 60 * 1000 },
          BASE + 2_000,
        ),
      ])
      expect(shape(rows)).toBe('[$1,$2]')
    })

    it('a sticker counts as an image', () => {
      const rows = groupMediaRuns([
        image('$1'),
        image('$2', {
          type: 'm.sticker',
          content: { body: 'wave', url: 'mxc://hs/s1' },
        } as unknown as Partial<TimelineEvent>),
      ])
      expect(shape(rows)).toBe('[$1,$2]')
    })

    it('read receipts are not consulted at all', () => {
      // Nothing in the input carries them; this pins the *absence* of a
      // break reason that would relayout on every ephemeral update.
      const rows = groupMediaRuns([image('$1'), image('$2')])
      expect(shape(rows)).toBe('[$1,$2]')
    })
  })

  describe('bounds', () => {
    it('splits a run longer than max', () => {
      const many = Array.from({ length: GALLERY_MAX + 3 }, (_, i) =>
        image(`$${i}`),
      )
      const rows = groupMediaRuns(many)
      expect(rows).toHaveLength(2)
      expect(rows[0].kind).toBe('gallery')
      expect((rows[0] as { events: TimelineEvent[] }).events).toHaveLength(
        GALLERY_MAX,
      )
      expect((rows[1] as { events: TimelineEvent[] }).events).toHaveLength(3)
    })

    it('flushes a max-split remainder of one as an ordinary row', () => {
      const many = Array.from({ length: GALLERY_MAX + 1 }, (_, i) =>
        image(`$${i}`),
      )
      const rows = groupMediaRuns(many)
      expect(rows[0].kind).toBe('gallery')
      expect(rows[1].kind).toBe('event')
    })

    it('honours an overridden min and window', () => {
      const rows = groupMediaRuns(
        [image('$1', {}, BASE), image('$2', {}, BASE + 5_000)],
        { min: 3 },
      )
      expect(shape(rows)).toBe('$1 $2')
    })
  })

  describe('keys', () => {
    it('keys a gallery on its first event, stable under prepend', () => {
      const rows = groupMediaRuns([image('$1'), image('$2')])
      expect(rows[0].key).toBe('gallery:$1')

      // A back-pagination prepend that does not join the run must not change
      // the existing row's key (WCR-01).
      const prepended = groupMediaRuns([
        text('$a', BASE - DAY),
        image('$1'),
        image('$2'),
      ])
      expect(prepended[1].key).toBe('gallery:$1')
    })

    it('keys ordinary rows on the event id', () => {
      expect(groupMediaRuns([text('$a')])[0].key).toBe('$a')
    })
  })

  describe('rowTs / rowLeadEvent', () => {
    it('reports the first event for a gallery', () => {
      const rows = groupMediaRuns([
        image('$1', {}, BASE),
        image('$2', {}, BASE + 1000),
      ])
      expect(rowTs(rows[0])).toBe(BASE)
      expect(rowLeadEvent(rows[0]).event_id).toBe('$1')
    })

    it('reports the event itself for an ordinary row', () => {
      const rows = groupMediaRuns([text('$a', BASE)])
      expect(rowTs(rows[0])).toBe(BASE)
      expect(rowLeadEvent(rows[0]).event_id).toBe('$a')
    })
  })

  describe('runPosition', () => {
    it('locates an image within its run', () => {
      const rows = groupMediaRuns([image('$1'), image('$2'), image('$3')])
      expect(runPosition(rows, '$2')).toEqual({ index: 1, total: 3 })
    })

    it('returns null for an image that is not in a gallery', () => {
      const rows = groupMediaRuns([text('$a'), image('$1'), text('$b')])
      expect(runPosition(rows, '$1')).toBeNull()
    })

    it('returns null for an unknown event', () => {
      expect(
        runPosition(groupMediaRuns([image('$1'), image('$2')]), '$x'),
      ).toBeNull()
    })
  })
})
