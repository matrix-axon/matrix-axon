import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  it,
  vi,
} from 'vitest'
import { ServicesContext } from '../services'
import type { TimelineEvent } from '../stores/timeline'
import { TEST_BASE_URL, testServices } from '../test/services'
import { EventBody } from '../components/EventBody'
import {
  AUTO_PAGE_LIMIT,
  MediaViewerProvider,
  type MediaViewerActions,
} from './media-viewer'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'
const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47])

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
})
afterAll(() => server.close())

/** Records every media path fetched, for the preload assertions. */
function serveBytes(): string[] {
  const requested: string[] = []
  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/media/:account/:server/:media`,
      ({ params }) => {
        requested.push(String(params.media))
        return new HttpResponse(PNG, {
          headers: { 'content-type': 'image/png' },
        })
      },
    ),
    http.get(
      `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
      ({ params }) => {
        requested.push(`${String(params.media)}:thumb`)
        return new HttpResponse(PNG, {
          headers: { 'content-type': 'image/png' },
        })
      },
    ),
  )
  return requested
}

function image(id: string, ts: number): TimelineEvent {
  return {
    account_id: ACCOUNT,
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
  } as unknown as TimelineEvent
}

function text(id: string, ts: number): TimelineEvent {
  return {
    ...image(id, ts),
    body: 'hi',
    content: { msgtype: 'm.text', body: 'hi' },
  } as unknown as TimelineEvent
}

/** A stand-in timeline: one row per event, each with a `data-event-id`. */
function Surface({
  events,
  onLoadOlder,
  atStart,
  runOf,
  actions,
}: {
  events: readonly TimelineEvent[]
  onLoadOlder?: () => Promise<boolean>
  atStart?: boolean
  runOf?: (eventId: string) => { index: number; total: number } | null
  actions?: MediaViewerActions
}) {
  return (
    <ServicesContext.Provider value={testServices()}>
      <MediaViewerProvider
        accountId={ACCOUNT}
        events={events}
        onLoadOlder={onLoadOlder}
        atStart={atStart}
        runOf={runOf}
        actions={actions}
        findRow={(eventId) =>
          [...document.querySelectorAll<HTMLElement>('[data-event-id]')].find(
            (el) => el.getAttribute('data-event-id') === eventId,
          ) ?? null
        }
      >
        <ol>
          {events.map((event) => (
            <li key={event.event_id} data-event-id={event.event_id}>
              <EventBody event={event} />
            </li>
          ))}
        </ol>
      </MediaViewerProvider>
    </ServicesContext.Provider>
  )
}

/** Click the row's open button once its bytes have resolved. */
async function openAt(
  container: Element,
  eventId: string,
): Promise<HTMLElement> {
  const row = [
    ...container.querySelectorAll<HTMLElement>('[data-event-id]'),
  ].find((el) => el.getAttribute('data-event-id') === eventId)!
  const button = await waitFor(() => {
    const found = row.querySelector<HTMLButtonElement>('.media-open')
    expect(found).not.toBeNull()
    expect(found!.disabled).toBe(false)
    return found!
  })
  fireEvent.click(button)
  return button
}

function status(): string {
  return document.querySelector('.lightbox-status')?.textContent ?? ''
}

/**
 * Which image the viewer is showing, by its accessible name.
 *
 * The counter only reports a position *within a gallery* now — an index among
 * every loaded image was dropped as useless to the reader — so tests that page
 * through standalone images identify them this way instead.
 */
function shownImage(): string {
  return dialog()?.getAttribute('aria-label') ?? ''
}

function dialog(): HTMLElement | null {
  return document.querySelector<HTMLElement>('[role="dialog"]')
}

/** Wait past `TOGGLE_GRACE_MS`, so the next tap counts as a fresh gesture. */
const pastToggleGrace = () => new Promise((resolve) => setTimeout(resolve, 320))

describe('MediaViewerProvider', () => {
  it('opens at the clicked image and reports its position', async () => {
    serveBytes()
    const events = [image('$1', 10), text('$t', 20), image('$2', 30)]
    const { container } = render(<Surface events={events} atStart />)

    await openAt(container, '$2')
    await waitFor(() => expect(dialog()).not.toBeNull())
    expect(shownImage()).toBe('$2.png')
  })

  it('keeps Close as the initial focus with contextual actions', async () => {
    serveBytes()
    const { container } = render(
      <Surface
        events={[image('$1', 10)]}
        atStart
        actions={{
          ownUserId: '@alice:hs',
          onReply: vi.fn(),
          onReact: vi.fn(),
          onOpenThread: vi.fn(),
          onDelete: vi.fn(async () => true),
        }}
      />,
    )

    await openAt(container, '$1')
    const close = document.querySelector<HTMLButtonElement>(
      '[aria-label="Close"]',
    )
    await waitFor(() => expect(document.activeElement).toBe(close))

    // The safe initial target is deliberately independent of the DOM/Tab
    // sequence: moving left from Close must follow the toolbar to Delete, then
    // React, rather than jumping around visually reordered controls.
    fireEvent.keyDown(document, { key: 'Tab', shiftKey: true })
    expect(document.activeElement).toBe(
      document.querySelector('[aria-label="Delete"]'),
    )
    fireEvent.keyDown(document, { key: 'Tab', shiftKey: true })
    expect(document.activeElement).toBe(
      document.querySelector('[aria-label="React"]'),
    )
  })

  it('delegates contextual actions for the displayed image after closing', async () => {
    serveBytes()
    const onReply = vi.fn()
    const onReact = vi.fn()
    const onOpenThread = vi.fn()
    const onDelete = vi.fn(async () => true)
    const { container } = render(
      <Surface
        events={[image('$1', 10), image('$2', 20)]}
        atStart
        actions={{
          ownUserId: '@alice:hs',
          onReply,
          onReact,
          onOpenThread,
          onDelete,
        }}
      />,
    )

    await openAt(container, '$2')
    const controls = [...document.querySelectorAll('.lightbox-toolbar button')]
    expect(
      controls.map((control) => control.getAttribute('aria-label')),
    ).toEqual(['Reply', 'Thread', 'React', 'Delete', 'Close'])

    fireEvent.click(
      document.querySelector<HTMLButtonElement>('[aria-label="Reply"]')!,
    )
    await waitFor(() => expect(dialog()).toBeNull())
    expect(onReply).toHaveBeenCalledWith(
      expect.objectContaining({ event_id: '$2' }),
    )

    await openAt(container, '$2')
    fireEvent.click(
      document.querySelector<HTMLButtonElement>('[aria-label="React"]')!,
    )
    await waitFor(() => expect(dialog()).toBeNull())
    expect(onReact).toHaveBeenCalledWith(
      expect.objectContaining({ event_id: '$2' }),
    )

    await openAt(container, '$2')
    fireEvent.click(
      document.querySelector<HTMLButtonElement>('[aria-label="Thread"]')!,
    )
    await waitFor(() => expect(dialog()).toBeNull())
    expect(onOpenThread).toHaveBeenCalledWith(
      expect.objectContaining({ event_id: '$2' }),
    )
  })

  it('confirms deletion of an own image and keeps a failed delete open', async () => {
    serveBytes()
    const onDelete = vi
      .fn<MediaViewerActions['onDelete']>()
      .mockResolvedValueOnce(false)
      .mockResolvedValueOnce(true)
    const { container } = render(
      <Surface
        events={[image('$1', 10)]}
        atStart
        actions={{
          ownUserId: '@alice:hs',
          onReply: vi.fn(),
          onReact: vi.fn(),
          onDelete,
        }}
      />,
    )

    await openAt(container, '$1')
    fireEvent.click(
      document.querySelector<HTMLButtonElement>('[aria-label="Delete"]')!,
    )
    expect(document.querySelector('[aria-label="Reply"]')).toBeNull()
    expect(
      document.querySelector('[aria-label="Cancel delete"]'),
    ).not.toBeNull()

    fireEvent.click(
      document.querySelector<HTMLButtonElement>(
        '[aria-label="Confirm delete"]',
      )!,
    )
    await waitFor(() =>
      expect(document.body.textContent).toContain('Delete failed'),
    )
    expect(dialog()).not.toBeNull()

    fireEvent.click(
      document.querySelector<HTMLButtonElement>(
        '[aria-label="Confirm delete"]',
      )!,
    )
    await waitFor(() => expect(dialog()).toBeNull())
    expect(onDelete).toHaveBeenCalledTimes(2)
  })

  it('does not offer deletion for another sender’s image', async () => {
    serveBytes()
    const otherImage = { ...image('$1', 10), sender: '@bob:hs' }
    const { container } = render(
      <Surface
        events={[otherImage]}
        atStart
        actions={{
          ownUserId: '@alice:hs',
          onReply: vi.fn(),
          onReact: vi.fn(),
          onDelete: vi.fn(async () => true),
        }}
      />,
    )

    await openAt(container, '$1')
    expect(document.querySelector('[aria-label="Delete"]')).toBeNull()
  })

  it('pages with arrow keys and stops inert at the newest end', async () => {
    serveBytes()
    const events = [image('$1', 10), image('$2', 20)]
    const { container } = render(<Surface events={events} atStart />)

    await openAt(container, '$1')
    await waitFor(() => expect(shownImage()).toBe('$1.png'))

    fireEvent.keyDown(document, { key: 'ArrowRight' })
    await waitFor(() => expect(shownImage()).toBe('$2.png'))

    // Newest end is inert: the button is disabled and the key is a no-op.
    const next = document.querySelector<HTMLButtonElement>('.lightbox-next')!
    expect(next.disabled).toBe(true)
    fireEvent.keyDown(document, { key: 'ArrowRight' })
    await waitFor(() => expect(shownImage()).toBe('$2.png'))
  })

  it('announces the oldest end when history is exhausted', async () => {
    serveBytes()
    const { container } = render(
      <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
    )
    await openAt(container, '$1')
    await waitFor(() => expect(status()).toContain('oldest image'))
    expect(
      document.querySelector<HTMLButtonElement>('.lightbox-prev')!.disabled,
    ).toBe(true)
  })

  it('still offers prev at the oldest loaded image when history remains', async () => {
    // The mirror of the test above. History that has not been paged in yet is
    // not "no history": disabling prev here would strand the reader one press
    // short of everything older, with nothing to say why.
    serveBytes()
    const { container } = render(
      <Surface
        events={[image('$1', 10), image('$2', 20)]}
        onLoadOlder={vi.fn().mockResolvedValue(true)}
      />,
    )
    await openAt(container, '$1')
    await waitFor(() => expect(dialog()).not.toBeNull())

    expect(
      document.querySelector<HTMLButtonElement>('.lightbox-prev')!.disabled,
    ).toBe(false)
    expect(status()).not.toContain('oldest image')
  })

  it('clicking prev or next does not dismiss the dialog', async () => {
    serveBytes()
    const { container } = render(
      <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
    )
    await openAt(container, '$1')
    await waitFor(() => expect(dialog()).not.toBeNull())

    fireEvent.click(document.querySelector('.lightbox-next')!)
    await waitFor(() => expect(shownImage()).toBe('$2.png'))
    expect(dialog()).not.toBeNull()
  })

  it('chains onLoadOlder past the oldest image and stops at the bound', async () => {
    serveBytes()
    // Every page is images-free, so the loop must keep asking — and must stop.
    const onLoadOlder = vi.fn().mockResolvedValue(true)
    const { container } = render(
      <Surface events={[image('$1', 10)]} onLoadOlder={onLoadOlder} />,
    )

    await openAt(container, '$1')
    await waitFor(() => expect(dialog()).not.toBeNull())
    fireEvent.keyDown(document, { key: 'ArrowLeft' })

    await waitFor(() =>
      expect(onLoadOlder.mock.calls.length).toBe(AUTO_PAGE_LIMIT),
    )
    // Bounded: it does not keep paging forever behind the overlay.
    await new Promise((resolve) => setTimeout(resolve, 20))
    expect(onLoadOlder.mock.calls.length).toBe(AUTO_PAGE_LIMIT)
  })

  it('takes the step on a single press once older images arrive', async () => {
    // Reported from real use: stepping back from the first loaded image did
    // nothing the first time and had to be pressed twice. Firing the load
    // changed a dependency of the pagination effect, which re-entered
    // synchronously, burned the whole page budget before anything resolved,
    // and cleared the pending intent — so the step was dropped.
    serveBytes()
    let events = [image('$2', 20), image('$3', 30)]
    let rerenderWith: (next: TimelineEvent[]) => void = () => {}
    // Deferred, like a real fetch. A load that resolves synchronously hides
    // the bug entirely: the sequence has already grown by the time the effect
    // re-enters, so it steps and nothing looks wrong.
    const onLoadOlder = vi.fn(async () => {
      await new Promise((resolve) => setTimeout(resolve, 10))
      events = [image('$1', 10), ...events]
      rerenderWith(events)
      return true
    })

    const view = render(<Surface events={events} onLoadOlder={onLoadOlder} />)
    rerenderWith = (next) =>
      view.rerender(<Surface events={next} onLoadOlder={onLoadOlder} />)

    await openAt(view.container, '$2')
    await waitFor(() => expect(shownImage()).toBe('$2.png'))

    // One press.
    fireEvent.keyDown(document, { key: 'ArrowLeft' })

    await waitFor(() => expect(shownImage()).toBe('$1.png'))
    // And exactly one page fetched, not the whole budget at once.
    expect(onLoadOlder).toHaveBeenCalledTimes(1)
  })

  it('gives a freshly opened image the whole page budget again', async () => {
    // The bound is per run of presses, not per session. Without a reset, a
    // reader who exhausted it once would find prev inert for the rest of the
    // room — and nothing on screen would explain why.
    serveBytes()
    const onLoadOlder = vi.fn().mockResolvedValue(true)
    const events = [image('$1', 10), image('$2', 20)]
    const { container } = render(
      <Surface events={events} onLoadOlder={onLoadOlder} />,
    )

    await openAt(container, '$1')
    await waitFor(() => expect(dialog()).not.toBeNull())
    fireEvent.keyDown(document, { key: 'ArrowLeft' })
    await waitFor(() =>
      expect(onLoadOlder.mock.calls.length).toBe(AUTO_PAGE_LIMIT),
    )

    fireEvent.keyDown(document, { key: 'Escape' })
    await waitFor(() => expect(dialog()).toBeNull())

    await openAt(container, '$1')
    await waitFor(() => expect(dialog()).not.toBeNull())
    fireEvent.keyDown(document, { key: 'ArrowLeft' })
    await waitFor(() =>
      expect(onLoadOlder.mock.calls.length).toBe(AUTO_PAGE_LIMIT * 2),
    )
  })

  it('does not page into history when already at the start', async () => {
    serveBytes()
    const onLoadOlder = vi.fn().mockResolvedValue(true)
    const { container } = render(
      <Surface events={[image('$1', 10)]} onLoadOlder={onLoadOlder} atStart />,
    )
    await openAt(container, '$1')
    await waitFor(() => expect(dialog()).not.toBeNull())
    fireEvent.keyDown(document, { key: 'ArrowLeft' })

    await new Promise((resolve) => setTimeout(resolve, 20))
    expect(onLoadOlder).not.toHaveBeenCalled()
  })

  it('keeps its place when older events are prepended', async () => {
    serveBytes()
    const { container, rerender } = render(
      <Surface events={[image('$2', 20)]} atStart />,
    )
    await openAt(container, '$2')
    await waitFor(() => expect(shownImage()).toBe('$2.png'))

    // A back-pagination prepend: the cursor tracks identity, so the open image
    // is still the open image — it has simply moved index.
    rerender(<Surface events={[image('$1', 10), image('$2', 20)]} atStart />)
    await new Promise((resolve) => setTimeout(resolve, 20))
    expect(shownImage()).toBe('$2.png')
  })

  it('keeps its place when a live event is appended', async () => {
    serveBytes()
    const { container, rerender } = render(
      <Surface events={[image('$1', 10)]} atStart />,
    )
    await openAt(container, '$1')
    await waitFor(() => expect(shownImage()).toBe('$1.png'))

    rerender(<Surface events={[image('$1', 10), image('$2', 20)]} atStart />)
    await waitFor(() => expect(shownImage()).toBe('$1.png'))
  })

  it('advances to the neighbour when the open event is redacted away', async () => {
    serveBytes()
    const { container, rerender } = render(
      <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
    )
    await openAt(container, '$2')
    await waitFor(() => expect(shownImage()).toBe('$2.png'))

    // $2 disappears; the viewer lands on a survivor rather than closing.
    rerender(<Surface events={[image('$1', 10)]} atStart />)
    await waitFor(() => expect(shownImage()).toBe('$1.png'))
    expect(dialog()).not.toBeNull()
  })

  it('lands on a sibling when the redacted image shared its timestamp', async () => {
    // A multi-image send is N events at the *same* `origin_ts`, so "nearest
    // newer" has to include the instant itself. A strict comparison finds no
    // candidate among the siblings and falls through to the last image in the
    // room — one redaction throwing the reader across the whole timeline.
    serveBytes()
    const { container, rerender } = render(
      <Surface
        events={[
          image('$a', 20),
          image('$b', 20),
          image('$c', 20),
          image('$later', 900),
        ]}
        atStart
      />,
    )
    await openAt(container, '$b')
    await waitFor(() => expect(shownImage()).toBe('$b.png'))

    rerender(
      <Surface
        events={[image('$a', 20), image('$c', 20), image('$later', 900)]}
        atStart
      />,
    )
    // The sibling it was sent with, not the newest image in the room.
    await waitFor(() => expect(shownImage()).not.toBe('$b.png'))
    expect(shownImage()).toBe('$a.png')
  })

  it('closes when the last image disappears', async () => {
    serveBytes()
    const { container, rerender } = render(
      <Surface events={[image('$1', 10)]} atStart />,
    )
    await openAt(container, '$1')
    await waitFor(() => expect(dialog()).not.toBeNull())

    rerender(<Surface events={[text('$t', 20)]} atStart />)
    await waitFor(() => expect(dialog()).toBeNull())
  })

  it('preloads both neighbours on open, before a direction is known', async () => {
    const requested = serveBytes()
    const events = [
      image('$1', 10),
      image('$2', 20),
      image('$3', 30),
      image('$4', 40),
      image('$5', 50),
    ]
    const { container } = render(<Surface events={events} atStart />)
    await openAt(container, '$3')
    await waitFor(() => expect(dialog()).not.toBeNull())

    await waitFor(() => {
      expect(requested).toContain('$2')
      expect(requested).toContain('$4')
    })
    // Two slots, never more: each is a full-size object competing with the
    // photo actually on screen.
    expect(requested).not.toContain('$5')
  })

  it('preloads nothing under Data Saver', async () => {
    // Preloading is a convenience; on a metered connection it is extra
    // full-size objects the reader never asked for.
    const requested = serveBytes()
    Object.defineProperty(navigator, 'connection', {
      value: { saveData: true },
      configurable: true,
    })
    try {
      const events = [image('$1', 10), image('$2', 20), image('$3', 30)]
      const { container } = render(<Surface events={events} atStart />)
      await openAt(container, '$2')
      await waitFor(() => expect(dialog()).not.toBeNull())
      await new Promise((resolve) => setTimeout(resolve, 20))

      // `$2` is there because the row itself fetched it to draw its own
      // thumbnail; the neighbours have no such excuse.
      expect(requested).not.toContain('$1')
      expect(requested).not.toContain('$3')
    } finally {
      Reflect.deleteProperty(navigator, 'connection')
    }
  })

  it('aims both preloads forward once the reader steps forward', async () => {
    // Symmetric ±1 wastes a slot — the image behind was just on screen and is
    // already parked in the zero-ref LRU, so re-requesting it buys nothing.
    const requested = serveBytes()
    const events = [
      image('$1', 10),
      image('$2', 20),
      image('$3', 30),
      image('$4', 40),
      image('$5', 50),
    ]
    const { container } = render(<Surface events={events} atStart />)
    await openAt(container, '$2')
    await waitFor(() => expect(dialog()).not.toBeNull())

    fireEvent.keyDown(document, { key: 'ArrowRight' })
    await waitFor(() => expect(shownImage()).toBe('$3.png'))

    await waitFor(() => {
      expect(requested).toContain('$4')
      expect(requested).toContain('$5')
    })
  })

  it('aims both preloads backward once the reader steps backward', async () => {
    const requested = serveBytes()
    const events = [
      image('$1', 10),
      image('$2', 20),
      image('$3', 30),
      image('$4', 40),
      image('$5', 50),
    ]
    const { container } = render(<Surface events={events} atStart />)
    await openAt(container, '$4')
    await waitFor(() => expect(dialog()).not.toBeNull())

    fireEvent.keyDown(document, { key: 'ArrowLeft' })
    await waitFor(() => expect(shownImage()).toBe('$3.png'))

    await waitFor(() => {
      expect(requested).toContain('$2')
      expect(requested).toContain('$1')
    })
  })

  it('pages on swipe: right reveals older, left reveals newer', async () => {
    serveBytes()
    const events = [image('$1', 10), image('$2', 20)]
    const { container } = render(<Surface events={events} atStart />)
    await openAt(container, '$2')
    await waitFor(() => expect(shownImage()).toBe('$2.png'))

    const overlay = dialog()!
    fireEvent.touchStart(overlay, { touches: [{ clientX: 200, clientY: 300 }] })
    fireEvent.touchEnd(overlay, {
      changedTouches: [{ clientX: 300, clientY: 305 }],
    })
    await waitFor(() => expect(shownImage()).toBe('$1.png'))

    fireEvent.touchStart(overlay, { touches: [{ clientX: 300, clientY: 300 }] })
    fireEvent.touchEnd(overlay, {
      changedTouches: [{ clientX: 200, clientY: 305 }],
    })
    await waitFor(() => expect(shownImage()).toBe('$2.png'))
  })

  it('dismisses on a downward swipe that starts on the image', async () => {
    serveBytes()
    const { container } = render(
      <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
    )
    await openAt(container, '$2')
    await waitFor(() => expect(dialog()).not.toBeNull())

    const overlay = dialog()!
    const target = document.querySelector('.lightbox-image')!
    fireEvent.touchStart(overlay, {
      touches: [{ clientX: 200, clientY: 200, target }],
    })
    fireEvent.touchEnd(overlay, {
      changedTouches: [{ clientX: 205, clientY: 360 }],
    })
    await waitFor(() => expect(dialog()).toBeNull())
  })

  it('does not dismiss on a downward swipe that starts off the image', async () => {
    // The same overlay hosts a PDF (ADR 0072), whose own vertical scrolling
    // would otherwise close the viewer under the reader.
    serveBytes()
    const { container } = render(
      <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
    )
    await openAt(container, '$2')
    await waitFor(() => expect(dialog()).not.toBeNull())

    const overlay = dialog()!
    fireEvent.touchStart(overlay, {
      touches: [{ clientX: 200, clientY: 200, target: overlay }],
    })
    fireEvent.touchEnd(overlay, {
      changedTouches: [{ clientX: 205, clientY: 360 }],
    })
    await new Promise((resolve) => setTimeout(resolve, 20))
    expect(dialog()).not.toBeNull()
  })

  it('does not dismiss on an upward swipe', async () => {
    serveBytes()
    const { container } = render(
      <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
    )
    await openAt(container, '$2')
    await waitFor(() => expect(dialog()).not.toBeNull())

    const overlay = dialog()!
    const target = document.querySelector('.lightbox-image')!
    fireEvent.touchStart(overlay, {
      touches: [{ clientX: 200, clientY: 360, target }],
    })
    fireEvent.touchEnd(overlay, {
      changedTouches: [{ clientX: 205, clientY: 200 }],
    })
    await new Promise((resolve) => setTimeout(resolve, 20))
    expect(dialog()).not.toBeNull()
  })

  it('declines a swipe starting in the native back edge band', async () => {
    serveBytes()
    const events = [image('$1', 10), image('$2', 20)]
    const { container } = render(<Surface events={events} atStart />)
    await openAt(container, '$2')
    await waitFor(() => expect(shownImage()).toBe('$2.png'))

    // WebKit's back recognizer is live over a fullscreen overlay, so the
    // viewer declines the same band the room does (ADR 0075).
    const overlay = dialog()!
    fireEvent.touchStart(overlay, { touches: [{ clientX: 5, clientY: 300 }] })
    fireEvent.touchEnd(overlay, {
      changedTouches: [{ clientX: 205, clientY: 305 }],
    })
    await new Promise((resolve) => setTimeout(resolve, 10))
    expect(shownImage()).toBe('$2.png')
  })

  it('ignores a mostly-vertical drag', async () => {
    serveBytes()
    const events = [image('$1', 10), image('$2', 20)]
    const { container } = render(<Surface events={events} atStart />)
    await openAt(container, '$2')
    await waitFor(() => expect(shownImage()).toBe('$2.png'))

    const overlay = dialog()!
    fireEvent.touchStart(overlay, { touches: [{ clientX: 200, clientY: 100 }] })
    fireEvent.touchEnd(overlay, {
      changedTouches: [{ clientX: 290, clientY: 400 }],
    })
    await new Promise((resolve) => setTimeout(resolve, 10))
    expect(shownImage()).toBe('$2.png')
  })

  describe('run-aware counter', () => {
    it('names the position within the post as well as the room', async () => {
      // Reported from real use: a five-image gallery read "4 of 8", because
      // back-pagination had pulled in three unrelated images. Paging is
      // deliberately global (ADR 0081), so the count is right — but it says
      // nothing to someone who just opened a five-image post.
      serveBytes()
      const events = [image('$1', 10), image('$2', 20), image('$3', 30)]
      const { container } = render(
        <Surface
          events={events}
          atStart
          runOf={(id) =>
            id === '$2' || id === '$3'
              ? { index: id === '$2' ? 0 : 1, total: 2 }
              : null
          }
        />,
      )

      await openAt(container, '$2')
      await waitFor(() => expect(status()).toContain('1/2'))
      // The index among *all* loaded images is deliberately absent: it moves
      // when back-pagination loads unrelated images, which reads as the post
      // having changed when nothing was sent.
      expect(status()).not.toContain('of 3')
    })

    it('falls back to the plain count outside a run', async () => {
      serveBytes()
      const { container } = render(
        <Surface
          events={[image('$1', 10), image('$2', 20)]}
          atStart
          runOf={() => null}
        />,
      )
      await openAt(container, '$1')
      await waitFor(() => expect(shownImage()).toBe('$1.png'))
      // Not in a gallery — no counter at all, just the caption if there is one.
      expect(status().replace('oldest image', '').trim()).toBe('')
    })
  })

  describe('saving to device', () => {
    const saveButton = () =>
      document.querySelector<HTMLButtonElement>('.lightbox-save')

    it('offers no save button until the bytes decode', async () => {
      // The proxy answers 200 with raw ciphertext when it lacks the key, so a
      // ready blob is not necessarily an image. Offering to save it would
      // write undecryptable bytes under a plausible `.png` name.
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openAt(container, '$1')
      const img = await waitFor(() => {
        const found = document.querySelector('.lightbox-image img')
        expect(found).not.toBeNull()
        return found!
      })
      // The blob resolved and the <img> is mounted, but nothing has decoded.
      expect(saveButton()).toBeNull()

      fireEvent.load(img)
      await waitFor(() => expect(saveButton()).not.toBeNull())
    })

    it('withdraws the save button when the image fails to decode', async () => {
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openAt(container, '$1')
      const img = await waitFor(() => {
        const found = document.querySelector('.lightbox-image img')
        expect(found).not.toBeNull()
        return found!
      })
      fireEvent.load(img)
      await waitFor(() => expect(saveButton()).not.toBeNull())

      fireEvent.error(img)
      await waitFor(() => expect(saveButton()).toBeNull())
      expect(document.querySelector('.lightbox-image')?.textContent).toContain(
        'could not decrypt',
      )
    })

    it('says so when the save fails, rather than looking like it worked', async () => {
      // `downloadMedia` resolves 'failed' on a fetch error. Reverting the
      // button to idle and saying nothing is indistinguishable from a save
      // that worked — the file simply never appears.
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openAt(container, '$1')
      const img = await waitFor(() => {
        const found = document.querySelector('.lightbox-image img')
        expect(found).not.toBeNull()
        return found!
      })
      fireEvent.load(img)
      await waitFor(() => expect(saveButton()).not.toBeNull())

      // The save re-fetches rather than reusing the displayed object, so it
      // is this request — not the one that drew the image — that fails.
      server.use(
        http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
          HttpResponse.error(),
        ),
      )
      fireEvent.click(saveButton()!)

      const alert = await waitFor(() => {
        const found = document.querySelector('.lightbox-save-error')
        expect(found).not.toBeNull()
        return found!
      })
      expect(alert.getAttribute('role')).toBe('alert')
      expect(alert.textContent).toContain('failed')
      // And the control comes back, so it can be retried.
      await waitFor(() => expect(saveButton()!.disabled).toBe(false))
    })

    it('does not carry a failed save onto the next image', async () => {
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openAt(container, '$1')
      const img = await waitFor(() => {
        const found = document.querySelector('.lightbox-image img')
        expect(found).not.toBeNull()
        return found!
      })
      fireEvent.load(img)
      await waitFor(() => expect(saveButton()).not.toBeNull())

      server.use(
        http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
          HttpResponse.error(),
        ),
      )
      fireEvent.click(saveButton()!)
      await waitFor(() =>
        expect(document.querySelector('.lightbox-save-error')).not.toBeNull(),
      )

      fireEvent.keyDown(document, { key: 'ArrowRight' })
      await waitFor(() => expect(shownImage()).toBe('$2.png'))
      // The failure belonged to the image being left behind.
      expect(document.querySelector('.lightbox-save-error')).toBeNull()
    })

    it('does not carry a failed save onto an auto-paginated image', async () => {
      // The other way onto a new image: stepping off the oldest loaded one
      // paginates history and lands on what arrives. That branch sets the
      // cursor itself, and used not to clear the failure — so the stale pill
      // rode along onto a photo the reader had not even tried to save.
      serveBytes()
      let events = [image('$2', 20), image('$3', 30)]
      let rerenderWith: (next: TimelineEvent[]) => void = () => {}
      const onLoadOlder = vi.fn(async () => {
        await new Promise((resolve) => setTimeout(resolve, 10))
        events = [image('$1', 10), ...events]
        rerenderWith(events)
        return true
      })
      const view = render(<Surface events={events} onLoadOlder={onLoadOlder} />)
      rerenderWith = (next) =>
        view.rerender(<Surface events={next} onLoadOlder={onLoadOlder} />)

      await openAt(view.container, '$2')
      const img = await waitFor(() => {
        const found = document.querySelector('.lightbox-image img')
        expect(found).not.toBeNull()
        return found!
      })
      fireEvent.load(img)
      await waitFor(() => expect(saveButton()).not.toBeNull())

      server.use(
        http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
          HttpResponse.error(),
        ),
      )
      fireEvent.click(saveButton()!)
      await waitFor(() =>
        expect(document.querySelector('.lightbox-save-error')).not.toBeNull(),
      )

      // Off the oldest end: a page is fetched and the viewer lands on $1.
      fireEvent.keyDown(document, { key: 'ArrowLeft' })
      await waitFor(() => expect(shownImage()).toBe('$1.png'))
      expect(document.querySelector('.lightbox-save-error')).toBeNull()
    })

    it('does not carry a failed save onto the image a redaction lands on', async () => {
      // The third way: the open event disappears and the viewer re-anchors on
      // a survivor. The reader never chose this image, so inheriting a failure
      // from the one that vanished is doubly wrong.
      serveBytes()
      const { container, rerender } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openAt(container, '$2')
      const img = await waitFor(() => {
        const found = document.querySelector('.lightbox-image img')
        expect(found).not.toBeNull()
        return found!
      })
      fireEvent.load(img)
      await waitFor(() => expect(saveButton()).not.toBeNull())

      server.use(
        http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
          HttpResponse.error(),
        ),
      )
      fireEvent.click(saveButton()!)
      await waitFor(() =>
        expect(document.querySelector('.lightbox-save-error')).not.toBeNull(),
      )

      rerender(<Surface events={[image('$1', 10)]} atStart />)
      await waitFor(() => expect(shownImage()).toBe('$1.png'))
      expect(document.querySelector('.lightbox-save-error')).toBeNull()
    })

    it('drops a save failure that resolves after the reader has paged on', async () => {
      // The mirror of the rule: clearing on arrival is not enough if a slow
      // failure can still arrive afterwards and accuse the new photo.
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openAt(container, '$1')
      const img = await waitFor(() => {
        const found = document.querySelector('.lightbox-image img')
        expect(found).not.toBeNull()
        return found!
      })
      fireEvent.load(img)
      await waitFor(() => expect(saveButton()).not.toBeNull())

      // Fails, but only after the reader has moved on.
      server.use(
        http.get(
          `${TEST_BASE_URL}/v1/media/:account/:server/:media`,
          async () => {
            await new Promise((resolve) => setTimeout(resolve, 30))
            return HttpResponse.error()
          },
        ),
      )
      fireEvent.click(saveButton()!)
      fireEvent.keyDown(document, { key: 'ArrowRight' })
      await waitFor(() => expect(shownImage()).toBe('$2.png'))

      await new Promise((resolve) => setTimeout(resolve, 60))
      expect(document.querySelector('.lightbox-save-error')).toBeNull()
    })

    it('does not carry one image’s decode verdict onto the next', async () => {
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openAt(container, '$1')
      const img = await waitFor(() => {
        const found = document.querySelector('.lightbox-image img')
        expect(found).not.toBeNull()
        return found!
      })
      fireEvent.load(img)
      await waitFor(() => expect(saveButton()).not.toBeNull())

      fireEvent.keyDown(document, { key: 'ArrowRight' })
      await waitFor(() => expect(shownImage()).toBe('$2.png'))
      // The next image has not decoded yet, so the offer is withdrawn again.
      expect(saveButton()).toBeNull()
    })
  })

  describe('immersive mode', () => {
    const overlay = () => document.querySelector('.lightbox')!

    async function openViewer(container: Element) {
      await openAt(container, '$1')
      await waitFor(() => expect(dialog()).not.toBeNull())
    }

    it('a tap on the image hides the chrome, and another brings it back', async () => {
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openViewer(container)
      expect(overlay().className).not.toContain('lightbox-immersive')

      fireEvent.click(document.querySelector('.lightbox-image')!)
      await waitFor(() =>
        expect(overlay().className).toContain('lightbox-immersive'),
      )

      await pastToggleGrace()
      fireEvent.click(document.querySelector('.lightbox-image')!)
      await waitFor(() =>
        expect(overlay().className).not.toContain('lightbox-immersive'),
      )
    })

    it('keeps the controls focusable while hidden, so Tab cannot escape', async () => {
      // Hiding with `display: none` would empty `collectFocusable`, and the
      // trap early-returns on an empty list — Tab would leave the dialog.
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openViewer(container)
      fireEvent.click(document.querySelector('.lightbox-image')!)
      await waitFor(() =>
        expect(overlay().className).toContain('lightbox-immersive'),
      )

      const close =
        document.querySelector<HTMLButtonElement>('.lightbox-close')!
      expect(close.hasAttribute('hidden')).toBe(false)
      expect(close.getAttribute('aria-hidden')).toBeNull()

      fireEvent.keyDown(document, { key: 'Tab' })
      expect(overlay().contains(document.activeElement)).toBe(true)
    })

    it('reveals the chrome again when a control takes focus', async () => {
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openViewer(container)
      fireEvent.click(document.querySelector('.lightbox-image')!)
      await waitFor(() =>
        expect(overlay().className).toContain('lightbox-immersive'),
      )

      // Entering immersive mode parks focus on the dialog, so no invisible
      // control holds it; tabbing to a real one reveals the chrome.
      expect(document.activeElement).toBe(overlay())

      document.querySelector<HTMLButtonElement>('.lightbox-close')!.focus()
      await waitFor(() =>
        expect(overlay().className).not.toContain('lightbox-immersive'),
      )
    })

    it('does not leave an invisible control holding focus', async () => {
      // Otherwise Enter or Space would fire the hidden ✕ and close the viewer
      // with nothing on screen to explain why.
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openViewer(container)
      expect(document.activeElement).toBe(
        document.querySelector('.lightbox-close'),
      )

      fireEvent.click(document.querySelector('.lightbox-image')!)
      await waitFor(() =>
        expect(overlay().className).toContain('lightbox-immersive'),
      )
      expect(document.activeElement).not.toBe(
        document.querySelector('.lightbox-close'),
      )
      expect(overlay().contains(document.activeElement)).toBe(true)
    })

    it('a double tap toggles once, not twice', async () => {
      // Reported from real use: the chrome flashed back and vanished again,
      // reading as "I cannot get the buttons to return". Two clicks
      // milliseconds apart — a double-click, or the impatient second tap
      // people give an unresponsive control — toggled twice. The second is
      // now swallowed, so the gesture settles instead of flickering.
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openViewer(container)

      const surface = document.querySelector('.lightbox-image')!
      fireEvent.click(surface)
      fireEvent.click(surface)
      await waitFor(() =>
        expect(overlay().className).toContain('lightbox-immersive'),
      )
      // Still hidden, not flickered back on and off.
      expect(overlay().className).toContain('lightbox-immersive')

      // A deliberate tap, after the grace, still works.
      await pastToggleGrace()
      fireEvent.click(surface)
      await waitFor(() =>
        expect(overlay().className).not.toContain('lightbox-immersive'),
      )
    })

    it('focus landing on the image itself does not reveal the chrome', async () => {
      // `.lightbox-image` is focusable (`tabindex="0"`), so a real browser
      // focuses it on mousedown. Treating that as "a control took focus" made
      // the reveal fire on every tap and undo the toggle the same click was
      // about to perform — immersive mode could be entered but never exited.
      // jsdom's synthetic click does not move focus, so only the e2e caught
      // it; this pins the guard.
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openViewer(container)
      fireEvent.click(document.querySelector('.lightbox-image')!)
      await waitFor(() =>
        expect(overlay().className).toContain('lightbox-immersive'),
      )

      fireEvent.focusIn(document.querySelector('.lightbox-image')!)
      expect(overlay().className).toContain('lightbox-immersive')
    })

    it('a tap on the image does not close the viewer', async () => {
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openViewer(container)
      fireEvent.click(document.querySelector('.lightbox-image')!)
      expect(dialog()).not.toBeNull()
    })

    it('the click synthesised after a swipe pages without toggling', async () => {
      serveBytes()
      const { container } = render(
        <Surface events={[image('$1', 10), image('$2', 20)]} atStart />,
      )
      await openViewer(container)
      await waitFor(() => expect(shownImage()).toBe('$1.png'))

      const el = overlay()
      fireEvent.touchStart(el, { touches: [{ clientX: 300, clientY: 300 }] })
      fireEvent.touchEnd(el, {
        changedTouches: [{ clientX: 200, clientY: 305 }],
      })
      await waitFor(() => expect(shownImage()).toBe('$2.png'))

      // The browser still fires a click on the element the touch ended over.
      fireEvent.click(document.querySelector('.lightbox-image')!)
      expect(overlay().className).not.toContain('lightbox-immersive')
    })
  })

  it('restores focus to a row whose image has not loaded yet', async () => {
    // Found live, not in jsdom: media loads lazily, so paging to an off-screen
    // image and closing lands on a row with no `.media-open` button. Focus
    // must still go there rather than being silently dropped on `<body>`.
    serveBytes()
    const events = [image('$1', 10), image('$2', 20)]
    const { container } = render(<Surface events={events} atStart />)

    await openAt(container, '$1')
    await waitFor(() => expect(shownImage()).toBe('$1.png'))
    fireEvent.keyDown(document, { key: 'ArrowRight' })
    await waitFor(() => expect(shownImage()).toBe('$2.png'))

    const row2 = [
      ...container.querySelectorAll<HTMLElement>('[data-event-id]'),
    ].find((el) => el.getAttribute('data-event-id') === '$2')!
    // Simulate the not-yet-loaded state the live client showed.
    row2.querySelector('.media-open')?.remove()

    fireEvent.keyDown(document, { key: 'Escape' })
    await waitFor(() => expect(dialog()).toBeNull())

    expect(document.activeElement).toBe(row2)
    expect(document.activeElement).not.toBe(document.body)
  })

  it('restores focus to the image it was left on, not the one it opened from', async () => {
    serveBytes()
    const events = [image('$1', 10), image('$2', 20)]
    const { container } = render(<Surface events={events} atStart />)

    const opener = await openAt(container, '$1')
    await waitFor(() => expect(shownImage()).toBe('$1.png'))
    fireEvent.keyDown(document, { key: 'ArrowRight' })
    await waitFor(() => expect(shownImage()).toBe('$2.png'))

    fireEvent.keyDown(document, { key: 'Escape' })
    await waitFor(() => expect(dialog()).toBeNull())

    const row2 = [
      ...container.querySelectorAll<HTMLElement>('[data-event-id]'),
    ].find((el) => el.getAttribute('data-event-id') === '$2')!
    expect(document.activeElement).toBe(row2.querySelector('.media-open'))
    expect(document.activeElement).not.toBe(opener)
  })
})
