import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { options } from 'preact'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, expect, it, vi } from 'vitest'
import { ServicesContext } from '../services'
import { TEST_BASE_URL, testServices } from '../test/services'
import {
  customReactionMxc,
  ReactionContent,
  reactionLabel,
} from './ReactionContent'

const MXC = 'mxc://hs/reaction'
const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
const server = setupServer()
const previousFrameScheduler = options.requestAnimationFrame
beforeAll(() => {
  // Async media completions can schedule effects outside testing-library's
  // act(). Drain them as microtasks, rather than leaving Preact's native frame
  // timeout alive after Vitest disposes this file's jsdom globals.
  options.requestAnimationFrame = queueMicrotask
  server.listen({ onUnhandledRequest: 'error' })
})
afterEach(() => {
  cleanup()
  server.resetHandlers()
  vi.restoreAllMocks()
})
afterAll(async () => {
  await Promise.resolve()
  options.requestAnimationFrame = previousFrameScheduler
  server.close()
})

function setup(reaction = MXC) {
  const services = testServices({ token: 'test-token' })
  const acquire = vi.spyOn(services.media, 'acquire')
  const view = render(
    <ServicesContext.Provider value={services}>
      <ReactionContent accountId="first" reaction={reaction} />
    </ServicesContext.Provider>,
  )
  return { ...view, services, acquire }
}

function serve(thumbnailStatus = 200) {
  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/media/:account/:server/:id/thumbnail`,
      ({ request }) => {
        expect(request.headers.get('Authorization')).toBe('Bearer test-token')
        const url = new URL(request.url)
        expect(url.searchParams.get('width')).toBe('32')
        expect(url.searchParams.get('height')).toBe('32')
        return new HttpResponse(PNG, {
          status: thumbnailStatus,
          headers: { 'Content-Type': 'image/png' },
        })
      },
    ),
    http.get(
      `${TEST_BASE_URL}/v1/media/:account/:server/:id`,
      () => new HttpResponse(PNG, { headers: { 'Content-Type': 'image/png' } }),
    ),
  )
}

it('accepts only structurally valid reaction MXCs within the server byte limit', () => {
  expect(customReactionMxc(MXC)).toBe(MXC)
  for (const key of [
    '👍',
    'https://tracking.test/a',
    'mxc:///a',
    'mxc://hs/',
    'mxc://hs/a/b',
    `mxc://hs/${'a'.repeat(2048)}`,
    `mxc://hs/${'é'.repeat(1024)}`,
  ]) {
    expect(customReactionMxc(key)).toBeNull()
  }
})

it('leaves Unicode and ordinary text reactions alone without media requests', () => {
  const view = setup('👍')
  expect(view.container.textContent).toBe('👍')
  expect(view.acquire).not.toHaveBeenCalled()
})

it('fetches authenticated thumbnails and scopes reused images to the account', async () => {
  serve()
  const view = setup()
  await waitFor(() =>
    expect(view.container.querySelector('img')?.src).toMatch(/^blob:/),
  )
  const first = view.container.querySelector('img')!.src
  view.rerender(
    <ServicesContext.Provider value={view.services}>
      <ReactionContent accountId="second" reaction={MXC} />
    </ServicesContext.Provider>,
  )
  // Never paint the previous account's blob while the new request is pending.
  expect(view.container.querySelector('img')).toBeNull()
  await waitFor(() =>
    expect(view.container.querySelector('img')?.src).toMatch(/^blob:/),
  )
  expect(view.container.querySelector('img')!.src).not.toBe(first)
  expect(view.acquire).toHaveBeenLastCalledWith('second', MXC, {
    thumbnail: { width: 32, height: 32, method: 'scale' },
    contentType: undefined,
  })
})

it('falls back to the original when a homeserver cannot thumbnail an image', async () => {
  serve(404)
  const view = setup()
  await waitFor(() =>
    expect(view.container.querySelector('img')).not.toBeNull(),
  )
  expect(view.acquire).toHaveBeenCalledTimes(2)
  expect(view.acquire).toHaveBeenLastCalledWith('first', MXC, undefined)
})

it('bounds decode recovery to the original and leaves a stable fallback', async () => {
  serve()
  const view = setup()
  await waitFor(() =>
    expect(view.container.querySelector('img')).not.toBeNull(),
  )
  const first = view.container.querySelector('img')!.src
  fireEvent.error(view.container.querySelector('img')!)
  await waitFor(() => {
    expect(view.container.querySelector('img')).not.toBeNull()
    expect(view.container.querySelector('img')!.src).not.toBe(first)
  })
  fireEvent.error(view.container.querySelector('img')!)
  await waitFor(() => expect(view.container.querySelector('img')).toBeNull())
  expect(view.container.querySelector('.reaction-image')?.textContent).toBe('◇')
  expect(view.acquire).toHaveBeenCalledTimes(2)
})

it('releases a late media result after the reaction disappears', async () => {
  const services = testServices()
  const release = vi.fn()
  let finish!: (handle: {
    result: { ok: true; url: string }
    release: () => void
  }) => void
  vi.spyOn(services.media, 'acquire').mockImplementation(
    () =>
      new Promise((resolve) => {
        finish = resolve
      }),
  )
  const view = render(
    <ServicesContext.Provider value={services}>
      <ReactionContent accountId="first" reaction={MXC} />
    </ServicesContext.Provider>,
  )
  await waitFor(() => expect(finish).toBeDefined())
  view.unmount()
  finish({ result: { ok: true, url: 'blob:late' }, release })
  await waitFor(() => expect(release).toHaveBeenCalledOnce())
})

it('uses only valid optional shortcodes for custom image labels', () => {
  expect(reactionLabel(MXC, ':partyparrot:')).toBe(':partyparrot:')
  expect(reactionLabel(MXC, '  parrot  ')).toBe('parrot')
  expect(reactionLabel(MXC, 'é'.repeat(50))).toBe('é'.repeat(50))
  for (const value of [
    undefined,
    null,
    12,
    {},
    [],
    '',
    '   ',
    'bad\nname',
    'x'.repeat(101),
    'é'.repeat(51),
  ]) {
    expect(reactionLabel(MXC, value)).toBe('Custom reaction')
  }
  expect(reactionLabel('👍', ':pretend:')).toBe('👍')
})

it('updates the image label without fetching the same image again', async () => {
  serve()
  const view = setup()
  await waitFor(() =>
    expect(view.container.querySelector('img')).not.toBeNull(),
  )
  const src = view.container.querySelector('img')!.src
  view.rerender(
    <ServicesContext.Provider value={view.services}>
      <ReactionContent
        accountId="first"
        reaction={MXC}
        shortcode="<b>parrot</b>"
      />
    </ServicesContext.Provider>,
  )
  expect(view.container.querySelector('img')!.alt).toBe('<b>parrot</b>')
  expect(view.container.querySelector('b')).toBeNull()
  expect(view.container.querySelector('img')!.src).toBe(src)
  expect(view.acquire).toHaveBeenCalledTimes(1)
})
