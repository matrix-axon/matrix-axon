import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import type { MembersStore } from '../stores/members'
import type { TimelineEvent } from '../stores/timeline'
import { TEST_BASE_URL, testServices } from '../test/services'
import { resetGalleryExpansion } from '../timeline/gallery-expansion'
import { GALLERY_EAGER_BYTES } from './MediaGalleryCell'
import { GALLERY_COLUMNS, MediaGalleryRow } from './MediaGalleryRow'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'
const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47])

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  resetGalleryExpansion()
})
afterAll(() => server.close())

function serveBytes(): string[] {
  const requested: string[] = []
  const respond = ({ params }: { params: Record<string, unknown> }) => {
    requested.push(String(params.media))
    return new HttpResponse(PNG, { headers: { 'content-type': 'image/png' } })
  }
  server.use(
    http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, respond),
    http.get(
      `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
      respond,
    ),
  )
  return requested
}

function image(
  id: string,
  content: Record<string, unknown> = {},
): TimelineEvent {
  return {
    account_id: ACCOUNT,
    event_id: id,
    room_id: '!r:hs',
    sender: '@alice:hs',
    origin_ts: 1000,
    arrival_order: 1000,
    type: 'm.room.message',
    body: `${id}.png`,
    content: {
      msgtype: 'm.image',
      body: `${id}.png`,
      url: `mxc://hs/${id}`,
      info: { mimetype: 'image/png', size: 10 },
      ...content,
    },
    redacted: false,
    edited: false,
    edit_count: 0,
    state_key: null,
    reactions: null,
    relates_to: null,
  } as unknown as TimelineEvent
}

/** An encrypted image with no sender thumbnail — the size-gated case. */
function encrypted(id: string, size: number | undefined): TimelineEvent {
  return image(id, {
    url: undefined,
    file: { url: `mxc://hs/${id}` },
    info: size === undefined ? {} : { size },
  })
}

/**
 * A still-uploading echo: it renders the composer's local preview and no
 * control, because there is nothing on the server to open yet.
 */
function pendingUpload(id: string): TimelineEvent {
  return {
    ...image(id),
    localEcho: {
      status: 'pending',
      body: `${id}.png`,
      options: {},
      media: {
        file: new File(['x'], `${id}.png`, { type: 'image/png' }),
        kind: 'image',
        previewUrl: `blob:${id}`,
      },
    },
  } as unknown as TimelineEvent
}

/** Encrypted, but the sender embedded a thumbnail — cheap despite its size. */
function encryptedWithThumbnail(id: string, size: number): TimelineEvent {
  return image(id, {
    url: undefined,
    file: { url: `mxc://hs/${id}` },
    info: { size, thumbnail_file: { url: `mxc://hs/${id}-thumb` } },
  })
}

const members = {
  displayName: (id: string) => id.replace(/^@|:hs$/g, ''),
  members: { value: new Map() },
} as unknown as MembersStore

function renderRow(events: TimelineEvent[], highlighted: string | null = null) {
  return render(
    <ServicesContext.Provider value={testServices()}>
      <ol>
        <MediaGalleryRow
          events={events}
          accountId={ACCOUNT}
          members={members}
          highlighted={highlighted}
          renderEvent={(event) => (
            <li class="stub-row" data-event-id={event.event_id}>
              {event.body}
            </li>
          )}
        />
      </ol>
    </ServicesContext.Provider>,
  )
}

describe('MediaGalleryRow', () => {
  it('is one li.event-row in the list, as scroll anchoring requires', () => {
    // `captureAnchor` binary-searches `li.event-row` assuming document order
    // is monotonic in position; a gallery that rendered otherwise would break
    // anchoring for every row below it.
    serveBytes()
    const { container } = renderRow([image('$1'), image('$2')])
    const rows = container.querySelectorAll('ol > li.event-row')
    expect(rows).toHaveLength(1)
    expect(rows[0].classList.contains('gallery-row')).toBe(true)
    expect(rows[0].getAttribute('data-event-id')).toBe('$1')
  })

  it('gives the grid real list semantics and a label', () => {
    serveBytes()
    const { container } = renderRow([image('$1'), image('$2'), image('$3')])
    const grid = container.querySelector('ul.gallery-grid')!
    expect(grid.getAttribute('aria-label')).toBe('3 images from alice')
    expect(grid.querySelectorAll(':scope > li')).toHaveLength(3)
  })

  it('renders captions beneath the grid, not on the cells', async () => {
    serveBytes()
    const { container } = renderRow([
      image('$1', { filename: 'a.png', body: 'Food' }),
      image('$2'),
    ])
    await waitFor(() =>
      expect(container.querySelector('.gallery-caption')).not.toBeNull(),
    )
    expect(container.querySelector('.gallery-caption')!.textContent).toContain(
      'Food',
    )
    expect(container.querySelector('.gallery-cell .gallery-caption')).toBeNull()
  })

  it('hands the column count to the CSS instead of repeating it', () => {
    // `ArrowUp`/`ArrowDown` move by `GALLERY_COLUMNS`; the grid used to repeat
    // the number as a CSS literal. Two independent constants that must agree
    // is a drift waiting to happen, so the grid reads this one.
    serveBytes()
    const { container } = renderRow([image('$1'), image('$2')])
    const grid = container.querySelector<HTMLElement>('ul.gallery-grid')!
    expect(grid.style.getPropertyValue('--gallery-columns')).toBe(
      String(GALLERY_COLUMNS),
    )
  })

  describe('the run byte budget', () => {
    const MB = 1024 * 1024

    it('loads a whole post of ordinary photos', async () => {
      // The reported failure: a per-image ceiling of 256 KB deferred every
      // cell once real camera photos arrived, so a gallery was a wall of
      // grey tiles. 2.6 + 1.3 + 1.9 + 1.7 MB is a real post and fits.
      const requested = serveBytes()
      const { container } = renderRow([
        encrypted('$1', 2.6 * MB),
        encrypted('$2', 1.3 * MB),
        encrypted('$3', 1.9 * MB),
        encrypted('$4', 1.7 * MB),
      ])
      expect(container.querySelector('.gallery-cell-deferred')).toBeNull()
      await waitFor(() => expect(requested.length).toBeGreaterThan(0))
    })

    it('defers only once the run passes the budget', () => {
      const { container } = renderRow([
        encrypted('$1', 5 * MB),
        encrypted('$2', 4 * MB),
        encrypted('$3', 4 * MB),
      ])
      // First two fit (5, then 9 > 8 stops the third onward).
      const deferred = container.querySelectorAll('.gallery-cell-deferred')
      expect(deferred).toHaveLength(1)
      expect(
        deferred[0].closest('.gallery-cell')?.previousElementSibling,
      ).not.toBeNull()
    })

    it('decides by position, so the first cells always fill in', () => {
      const many = Array.from({ length: 8 }, (_, i) =>
        encrypted(`$${i}`, 2 * MB),
      )
      const { container } = renderRow(many)
      const cells = [...container.querySelectorAll('.gallery-cell')]
      const isDeferred = cells.map(
        (c) => c.querySelector('.gallery-cell-deferred') !== null,
      )
      // A prefix loads, the tail defers — never interleaved.
      const firstDeferred = isDeferred.indexOf(true)
      expect(firstDeferred).toBeGreaterThan(0)
      expect(isDeferred.slice(firstDeferred).every(Boolean)).toBe(true)
    })

    it('spends an unknown size at full weight, because unknown might be huge', () => {
      const { container } = renderRow([
        encrypted('$1', undefined),
        encrypted('$2', 1024),
      ])
      // The first exhausts the budget on its own.
      expect(container.querySelectorAll('.gallery-cell-deferred')).toHaveLength(
        1,
      )
    })

    it('never spends budget on media that is already cheap', () => {
      // Plaintext: the server can thumbnail it, so it costs the run nothing.
      const { container } = renderRow([
        image('$1', { info: { mimetype: 'image/png', size: 5 * MB } }),
        image('$2', { info: { mimetype: 'image/png', size: 5 * MB } }),
        encrypted('$3', 7 * MB),
      ])
      expect(container.querySelector('.gallery-cell-deferred')).toBeNull()
    })

    it('names what a deferred cell will do, and fetches once asked', async () => {
      const requested = serveBytes()
      const { container } = renderRow([
        encrypted('$1', 9 * MB),
        encrypted('$2', 2 * MB),
      ])
      const tile = container.querySelector<HTMLButtonElement>(
        '.gallery-cell-deferred',
      )!
      expect(tile.getAttribute('aria-label')).toContain('Load image 2 of 2')
      expect(tile.getAttribute('aria-label')).toContain('2.0 MB')

      fireEvent.click(tile)
      await waitFor(() => expect(requested).toContain('$2'))
    })

    it('never defers an image the sender thumbnailed, however large', () => {
      // The budget exists to bound *full-size* fetches. An embedded thumbnail
      // is a few KB, so gating it on the parent object's size would defer a
      // cell that was already cheap — and leave a grey tile with a thumbnail
      // sitting unused beside it.
      const { container } = renderRow([
        encrypted('$1', 9 * MB),
        encryptedWithThumbnail('$2', 9 * MB),
      ])
      const deferred = [
        ...container.querySelectorAll('.gallery-cell-deferred'),
      ].map((el) => el.closest('.gallery-cell')?.getAttribute('data-event-id'))
      expect(deferred).toEqual([])
    })

    it('keeps the budget within an order of magnitude of a real post', () => {
      // A guard against re-tuning this into uselessness in either direction.
      expect(GALLERY_EAGER_BYTES).toBeGreaterThan(4 * MB)
      expect(GALLERY_EAGER_BYTES).toBeLessThan(64 * MB)
    })
  })

  describe('the ungroup control', () => {
    it('renders the run as ordinary rows and back', async () => {
      serveBytes()
      const { container } = renderRow([image('$1'), image('$2'), image('$3')])
      const toggle = () =>
        container.querySelector<HTMLButtonElement>('.gallery-expander')!

      expect(toggle().getAttribute('aria-expanded')).toBe('false')
      fireEvent.click(toggle())

      await waitFor(() =>
        expect(container.querySelectorAll('.stub-row')).toHaveLength(3),
      )
      expect(container.querySelector('.gallery-grid')).toBeNull()
      expect(toggle().getAttribute('aria-expanded')).toBe('true')

      fireEvent.click(toggle())
      await waitFor(() =>
        expect(container.querySelector('.gallery-grid')).not.toBeNull(),
      )
      expect(container.querySelectorAll('.stub-row')).toHaveLength(0)
    })

    it('remembers the choice across a remount', async () => {
      // Switching rooms unmounts the timeline; the choice must survive it.
      serveBytes()
      const events = [image('$1'), image('$2')]
      const first = renderRow(events)
      fireEvent.click(
        first.container.querySelector<HTMLButtonElement>('.gallery-expander')!,
      )
      await waitFor(() =>
        expect(first.container.querySelectorAll('.stub-row')).toHaveLength(2),
      )
      cleanup()
      const second = renderRow(events)
      expect(second.container.querySelectorAll('.stub-row')).toHaveLength(2)
      expect(second.container.querySelector('.gallery-grid')).toBeNull()
    })
  })

  describe('the thumbnail a cell asks for', () => {
    /** Every media url requested, thumbnails carrying their query. */
    function recordUrls(thumbnailStatus: number): string[] {
      const urls: string[] = []
      server.use(
        http.get(
          `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
          ({ request, params }) => {
            const query = new URL(request.url).searchParams
            urls.push(`${String(params.media)}:thumb?${query.toString()}`)
            return thumbnailStatus === 200
              ? new HttpResponse(PNG, {
                  headers: { 'content-type': 'image/png' },
                })
              : new HttpResponse(null, { status: thumbnailStatus })
          },
        ),
        http.get(
          `${TEST_BASE_URL}/v1/media/:account/:server/:media`,
          ({ params }) => {
            urls.push(String(params.media))
            return new HttpResponse(PNG, {
              headers: { 'content-type': 'image/png' },
            })
          },
        ),
      )
      return urls
    }

    it('asks for a crop, which cannot collide with the scaled inline one', async () => {
      // The grid is square by construction, and `keyOf` in `media-service`
      // includes the method — a cell asking for `scale` would both letterbox
      // and share a cache slot with `MediaImage`.
      const urls = recordUrls(200)
      renderRow([image('$1'), image('$2')])
      await waitFor(() => expect(urls.length).toBe(2))
      for (const url of urls) {
        expect(url).toContain('method=crop')
      }
    })

    it('falls back to the full-size object when the thumbnail fails', async () => {
      // Shipped broken: the cell passed a constant `'idle'` into
      // `useThumbnailFallback`, so the shared recovery — the whole reason the
      // hook was extracted from `MediaImage` — was inert here and a
      // homeserver that cannot crop left an empty tile.
      const urls = recordUrls(404)
      const { container } = renderRow([image('$1'), image('$2')])

      await waitFor(() =>
        expect(container.querySelectorAll('.gallery-cell-open')).toHaveLength(
          2,
        ),
      )
      expect(container.querySelector('.gallery-cell-failed')).toBeNull()
      // Both the rejected thumbnail and the full-size object it recovered to.
      expect(urls.filter((url) => url === '$1')).toHaveLength(1)
      expect(urls.filter((url) => url.startsWith('$1:thumb'))).toHaveLength(1)
    })
  })

  describe('roving tabindex', () => {
    it('makes the grid a single tab stop', async () => {
      const requested = serveBytes()
      const { container } = renderRow([image('$1'), image('$2'), image('$3')])
      await waitFor(() => expect(requested.length).toBe(3))
      await waitFor(() =>
        expect(container.querySelectorAll('.gallery-cell-open')).toHaveLength(
          3,
        ),
      )

      const cells = [
        ...container.querySelectorAll<HTMLElement>('.gallery-cell-open'),
      ]
      expect(cells.map((c) => c.tabIndex)).toEqual([0, -1, -1])
    })

    it('moves focus with the arrow keys', async () => {
      const requested = serveBytes()
      const { container } = renderRow([image('$1'), image('$2'), image('$3')])
      await waitFor(() =>
        expect(container.querySelectorAll('.gallery-cell-open')).toHaveLength(
          3,
        ),
      )
      const cells = [
        ...container.querySelectorAll<HTMLElement>('.gallery-cell-open'),
      ]

      cells[0].focus()
      fireEvent.keyDown(cells[0], { key: 'ArrowRight' })
      await waitFor(() => expect(document.activeElement).toBe(cells[1]))
      expect(requested.length).toBeGreaterThan(0)

      fireEvent.keyDown(cells[1], { key: 'ArrowLeft' })
      await waitFor(() => expect(document.activeElement).toBe(cells[0]))
    })

    it('steps over a pending cell instead of losing the reader on it', async () => {
      // A ready/pending/ready run. The middle event is still uploading, so it
      // renders no control. Counting positions in `events` rather than in the
      // cells that can take focus put logical focus on the pending event and
      // actual focus on the *third* cell, and every step after that was
      // misaligned.
      const requested = serveBytes()
      const { container } = renderRow([
        image('$1'),
        pendingUpload('$2'),
        image('$3'),
      ])
      await waitFor(() => expect(requested.length).toBeGreaterThan(0))
      await waitFor(() =>
        expect(container.querySelectorAll('.gallery-cell-open')).toHaveLength(
          2,
        ),
      )
      const cells = [
        ...container.querySelectorAll<HTMLElement>('.gallery-cell-open'),
      ]
      const idOf = (el: HTMLElement) =>
        el.closest('.gallery-cell')?.getAttribute('data-event-id')
      expect(cells.map(idOf)).toEqual(['$1', '$3'])

      cells[0].focus()
      fireEvent.keyDown(cells[0], { key: 'ArrowRight' })
      await waitFor(() => expect(document.activeElement).toBe(cells[1]))
      expect(idOf(document.activeElement as HTMLElement)).toBe('$3')

      // And back, rather than into the pending cell's dead ground.
      fireEvent.keyDown(cells[1], { key: 'ArrowLeft' })
      await waitFor(() => expect(document.activeElement).toBe(cells[0]))
    })

    it('gives the grid a tab stop even when the first image is still uploading', async () => {
      // The tab stop cannot live on a cell that renders no control: the grid
      // would drop out of the tab order entirely and be unreachable.
      const requested = serveBytes()
      const { container } = renderRow([pendingUpload('$1'), image('$2')])
      await waitFor(() => expect(requested.length).toBeGreaterThan(0))
      const stop = await waitFor(() => {
        const found = container.querySelector<HTMLElement>('.gallery-cell-open')
        expect(found).not.toBeNull()
        return found!
      })
      expect(stop.tabIndex).toBe(0)
      expect(stop.closest('.gallery-cell')?.getAttribute('data-event-id')).toBe(
        '$2',
      )
    })

    it('does not wrap past either end', async () => {
      serveBytes()
      const { container } = renderRow([image('$1'), image('$2')])
      await waitFor(() =>
        expect(container.querySelectorAll('.gallery-cell-open')).toHaveLength(
          2,
        ),
      )
      const cells = [
        ...container.querySelectorAll<HTMLElement>('.gallery-cell-open'),
      ]

      cells[0].focus()
      fireEvent.keyDown(cells[0], { key: 'ArrowLeft' })
      expect(document.activeElement).toBe(cells[0])

      cells[1].focus()
      fireEvent.keyDown(cells[1], { key: 'ArrowRight' })
      expect(document.activeElement).toBe(cells[1])

      // Focus not moving is not enough: an out-of-range press that still
      // recorded the position would leave the roving tabindex pointing at no
      // cell, and the grid would drop out of the tab order entirely.
      expect(cells.filter((cell) => cell.tabIndex === 0)).toHaveLength(1)
    })
  })

  it('names each cell with its position', async () => {
    serveBytes()
    const { container } = renderRow([image('$1'), image('$2')])
    await waitFor(() =>
      expect(container.querySelectorAll('.gallery-cell-open')).toHaveLength(2),
    )
    const labels = [...container.querySelectorAll('.gallery-cell-open')].map(
      (c) => c.getAttribute('aria-label'),
    )
    expect(labels[0]).toContain('Image 1 of 2')
    expect(labels[1]).toContain('Image 2 of 2')
  })

  describe('a deep link into the run', () => {
    it('keeps the gallery intact and highlights the cell', async () => {
      // Forcing the jump target back to an ordinary row was the first
      // approach; it tore a gallery into up to three rows for a link that
      // pointed *into* it.
      serveBytes()
      const { container } = renderRow(
        [image('$1'), image('$2'), image('$3')],
        '$2',
      )
      await waitFor(() =>
        expect(container.querySelector('.gallery-grid')).not.toBeNull(),
      )
      expect(container.querySelectorAll('.gallery-cell')).toHaveLength(3)

      const highlighted = container.querySelectorAll(
        '.gallery-cell.highlighted',
      )
      expect(highlighted).toHaveLength(1)
      expect(highlighted[0].getAttribute('data-event-id')).toBe('$2')
    })

    it('gives every cell its own data-event-id, so the jump can find it', () => {
      // `centerHighlightedRow` scans `[data-event-id]`; without this it would
      // only ever find the run's first event.
      serveBytes()
      const { container } = renderRow([image('$1'), image('$2'), image('$3')])
      const ids = [...container.querySelectorAll('.gallery-cell')].map((c) =>
        c.getAttribute('data-event-id'),
      )
      expect(ids).toEqual(['$1', '$2', '$3'])
    })

    it('does not make a cell look like a timeline row to scroll anchoring', () => {
      // `captureAnchor` binary-searches `li.event-row` assuming document order
      // is monotonic in position. Cells carry `data-event-id` but must not
      // carry that class, or every gallery would inject phantom rows.
      serveBytes()
      const { container } = renderRow([image('$1'), image('$2')])
      expect(container.querySelectorAll('li.event-row')).toHaveLength(1)
      expect(
        container.querySelectorAll('.gallery-cell.event-row'),
      ).toHaveLength(0)
    })
  })

  it('shows receipts once, beneath the grid', async () => {
    serveBytes()
    const { container } = render(
      <ServicesContext.Provider value={testServices()}>
        <ol>
          <MediaGalleryRow
            events={[image('$1'), image('$2')]}
            accountId={ACCOUNT}
            members={members}
            readReceipts={[{ userId: '@bob:hs', ts: 1 }]}
            renderEvent={() => null}
          />
        </ol>
      </ServicesContext.Provider>,
    )
    const receipts = container.querySelectorAll('.read-receipts')
    expect(receipts).toHaveLength(1)
    expect(receipts[0].textContent).toContain('bob')
  })

  it('expands the truncated summary to every viewer, even in a large room', async () => {
    serveBytes()
    const viewers = Array.from({ length: 12 }, (_, i) => ({
      userId: `@user${i}:hs`,
      ts: i,
    }))
    const { container } = render(
      <ServicesContext.Provider value={testServices()}>
        <ol>
          <MediaGalleryRow
            events={[image('$1'), image('$2')]}
            accountId={ACCOUNT}
            members={members}
            readReceipts={viewers}
            renderEvent={() => null}
          />
        </ol>
      </ServicesContext.Provider>,
    )
    expect(container.querySelector('.read-receipts')?.textContent).toContain(
      '10 more',
    )
    expect(container.querySelector('.read-receipts-popover')).toBeNull()

    fireEvent.click(container.querySelector('.read-receipts')!)

    const popover = container.querySelector('.read-receipts-popover')
    expect(popover).not.toBeNull()
    expect(popover!.querySelectorAll('.read-receipts-row')).toHaveLength(12)
    expect(popover!.textContent).toContain('user11')
  })
})
