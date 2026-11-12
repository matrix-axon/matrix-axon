import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import {
  afterAll,
  afterEach,
  beforeAll,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from 'vitest'
import { createApiClient } from './api/client'
import {
  createComposerLinkPreviews,
  createLinkPreviewCache,
  fromDevice,
  homeserverLinkPreview,
  PreviewLookupError,
  previewFailureMessage,
  type ComposerLinkPreviews,
  type PreviewResult,
  type PreviewSources,
} from './composer-link-previews'
import { LinkPreviewError, type DeviceLinkPreview } from './platform'

const A = 'https://a.example.org/x'
const B = 'https://b.example.org/y'
const C = 'https://c.example.org/z'
const D = 'https://d.example.org/w'

function devicePreview(url: string): DeviceLinkPreview {
  return { url, title: `Title ${url}`, imageUnavailable: false }
}

/** A device source whose lookups the test settles by hand. */
function deviceSource() {
  const calls: {
    url: string
    signal: AbortSignal
    resolve: (preview: DeviceLinkPreview) => void
    reject: (error: unknown) => void
  }[] = []
  const device = vi.fn(
    (url: string, signal: AbortSignal) =>
      new Promise<DeviceLinkPreview>((resolve, reject) => {
        calls.push({ url, signal, resolve, reject })
      }),
  )
  return { device, calls }
}

const result = (title: string): PreviewResult => ({
  title,
  description: null,
  siteName: null,
  image: null,
  imageLost: false,
})

let controller: ComposerLinkPreviews
beforeEach(() => vi.useFakeTimers())
afterEach(() => {
  controller?.dispose()
  vi.useRealTimers()
})

function make(sources: PreviewSources, initial = '') {
  controller = createComposerLinkPreviews(createLinkPreviewCache())
  controller.reset(initial)
  controller.configure(sources, false)
  return controller
}

const statuses = () =>
  controller.cards.value.map((card) => [card.matchedUrl, card.state.status])

describe('typed and pasted links (§ 5)', () => {
  it('starts a typed link only once whitespace follows it, after the delay', async () => {
    const { device, calls } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`see ${A}`)
    expect(statuses()).toEqual([])
    controller.edit(`see ${A} `)
    expect(statuses()).toEqual([[A, 'waiting']])
    await vi.advanceTimersByTimeAsync(399)
    expect(device).not.toHaveBeenCalled()
    await vi.advanceTimersByTimeAsync(1)
    expect(device).toHaveBeenCalledWith(A, expect.any(AbortSignal))
    expect(statuses()).toEqual([[A, 'loading']])
    calls[0].resolve(devicePreview(A))
    await vi.advanceTimersByTimeAsync(0)
    expect(statuses()).toEqual([[A, 'ready']])
  })

  it('never looks up the partial links typed on the way', async () => {
    const { device } = deviceSource()
    make({ setting: 'device', device })
    for (let i = 'https://'.length; i <= A.length; i += 1)
      controller.edit(A.slice(0, i))
    controller.edit(`${A}\n`)
    await vi.advanceTimersByTimeAsync(400)
    expect(device.mock.calls.map(([url]) => url)).toEqual([A])
  })

  it('looks up a pasted link at once, even with nothing after it', () => {
    const { device } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`see ${A}`, true)
    expect(device).toHaveBeenCalledTimes(1)
    expect(statuses()).toEqual([[A, 'loading']])
  })

  it('cancels a typed link removed within the delay', async () => {
    const { device } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`${A} `)
    controller.edit('')
    await vi.advanceTimersByTimeAsync(1000)
    expect(device).not.toHaveBeenCalled()
    expect(statuses()).toEqual([])
  })

  it('cancels a lookup and removes its card when the link is removed', () => {
    const { device, calls } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`${A} `, true)
    controller.edit(' ')
    expect(calls[0].signal.aborted).toBe(true)
    expect(statuses()).toEqual([])
  })

  it('treats a changed link as a new one', async () => {
    const { device } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`${A} `, true)
    controller.edit(`${A}/more `)
    await vi.advanceTimersByTimeAsync(400)
    expect(device.mock.calls.map(([url]) => url)).toEqual([A, `${A}/more`])
  })
})

describe('restored drafts (§ 5)', () => {
  it('gives a restored link no card, even after other edits', async () => {
    const { device } = deviceSource()
    make({ setting: 'device', device }, `draft ${A}`)
    controller.edit(`draft ${A} and more`)
    controller.edit(`edited draft ${A} and more `)
    await vi.advanceTimersByTimeAsync(1000)
    expect(device).not.toHaveBeenCalled()
    expect(statuses()).toEqual([])
  })

  it('adds it once the user deletes it and types it again', async () => {
    const { device } = deviceSource()
    make({ setting: 'device', device }, `draft ${A} `)
    controller.edit('draft  ')
    controller.edit(`draft ${A} `)
    await vi.advanceTimersByTimeAsync(400)
    expect(device).toHaveBeenCalledWith(A, expect.any(AbortSignal))
  })

  it('looks up a new link typed beside a restored one', async () => {
    const { device } = deviceSource()
    make({ setting: 'device', device }, `${A} `)
    controller.edit(`${A} ${B} `)
    await vi.advanceTimersByTimeAsync(400)
    expect(device.mock.calls.map(([url]) => url)).toEqual([B])
    expect(statuses()).toEqual([[B, 'loading']])
  })

  it('treats a draft adopted later as restored too', async () => {
    const { device } = deviceSource()
    make({ setting: 'device', device })
    controller.reset(`from another device ${A} `)
    controller.edit(`from another device ${A} ok`)
    await vi.advanceTimersByTimeAsync(400)
    expect(device).not.toHaveBeenCalled()
  })
})

describe('discards, slots and identity (§ 1, § 5)', () => {
  it('remembers a discarded link for the rest of the draft', async () => {
    const { device, calls } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`${A} `, true)
    const [{ identity }] = controller.cards.value
    controller.discard(identity)
    expect(calls[0].signal.aborted).toBe(true)
    expect(statuses()).toEqual([])
    controller.edit(' ')
    controller.edit(`${A} `, true)
    await vi.advanceTimersByTimeAsync(400)
    expect(device).toHaveBeenCalledTimes(1)
    expect(statuses()).toEqual([])
    expect(controller.hasDiscardedLink(`text ${A}`)).toBe(true)
    // A new draft forgets it.
    expect(controller.take().discarded).toEqual(new Set([A]))
    controller.edit(`${A} `, true)
    expect(device).toHaveBeenCalledTimes(2)
  })

  it('discards every occurrence by identity', () => {
    const { device } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`${A} and https://A.example.org:443/x `, true)
    expect(device).toHaveBeenCalledTimes(1)
    expect(statuses()).toEqual([[A, 'loading']])
    controller.discard(controller.cards.value[0].identity)
    expect(controller.hasDiscardedLink('https://A.EXAMPLE.org/x')).toBe(true)
  })

  it('shares one card between spellings, keyed by the first occurrence', () => {
    const { device } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`https://A.example.org/x and ${A} `, true)
    expect(statuses()).toEqual([['https://A.example.org/x', 'loading']])
    controller.edit(`${A} `)
    expect(statuses()).toEqual([[A, 'loading']])
    expect(device).toHaveBeenCalledTimes(1)
  })

  it('gives three links cards and the fourth none', () => {
    const { device } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`${A} ${B} ${C} ${D} `, true)
    expect(statuses().map(([url]) => url)).toEqual([A, B, C])
    expect(device).toHaveBeenCalledTimes(3)
  })

  it('does not give a discarded slot to a later link', () => {
    const { device } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`${A} ${B} ${C} ${D} `, true)
    controller.discard(controller.cards.value[1].identity)
    expect(statuses().map(([url]) => url)).toEqual([A, C])
    expect(device).toHaveBeenCalledTimes(3)
  })

  it('counts a restored link against the three', () => {
    const { device } = deviceSource()
    make({ setting: 'device', device }, `${A} ${B} ${C} `)
    controller.edit(`${A} ${B} ${C} ${D} `, true)
    expect(statuses()).toEqual([])
    expect(device).not.toHaveBeenCalled()
  })
})

describe('results and failures', () => {
  it('reuses a cached result instead of fetching again', async () => {
    const cache = createLinkPreviewCache()
    const { device, calls } = deviceSource()
    controller = createComposerLinkPreviews(cache)
    controller.configure({ setting: 'device', device }, false)
    controller.edit(`${A} `, true)
    calls[0].resolve(devicePreview(A))
    await vi.advanceTimersByTimeAsync(0)
    controller.take()
    const second = createComposerLinkPreviews(cache)
    second.configure({ setting: 'device', device }, false)
    second.edit(`${A} `, true)
    expect(device).toHaveBeenCalledTimes(1)
    expect(second.cards.value[0].state.status).toBe('ready')
    second.dispose()
  })

  it('evicts the least recently used result past the limit', () => {
    const cache = createLinkPreviewCache(2)
    cache.writer()('a', result('a'))
    cache.writer()('b', result('b'))
    cache.get('a')
    cache.writer()('c', result('c'))
    expect(cache.get('b')).toBeUndefined()
    expect(cache.get('a')).toEqual(result('a'))
    cache.clear()
    expect(cache.get('a')).toBeUndefined()
  })

  it('drops a write from a writer taken before a clear', () => {
    const cache = createLinkPreviewCache()
    const before = cache.writer()
    cache.clear()
    before('a', result('a'))
    expect(cache.get('a')).toBeUndefined()
    cache.writer()('a', result('a'))
    expect(cache.get('a')).toEqual(result('a'))
  })

  it('does not cache a result that was in flight when the cache was cleared', async () => {
    // A sign-out clears the cache while a lookup is still out (review of PR 3).
    const cache = createLinkPreviewCache()
    const { device, calls } = deviceSource()
    controller = createComposerLinkPreviews(cache)
    controller.configure({ setting: 'device', device }, false)
    controller.edit(`${A} `, true)
    cache.clear()
    calls[0].resolve(devicePreview(A))
    await vi.advanceTimersByTimeAsync(0)
    expect(cache.get(A)).toBeUndefined()
  })

  it('shows a device failure, and offers the homeserver only when allowed', async () => {
    const { device, calls } = deviceSource()
    const homeserver = vi.fn(async () => result('From the homeserver'))
    make({ setting: 'device', device, homeserver })
    controller.edit(`${A} `, true)
    calls[0].reject(new LinkPreviewError('public_ipv4_required'))
    await vi.advanceTimersByTimeAsync(0)
    expect(controller.cards.value[0]).toMatchObject({
      state: {
        status: 'failed',
        failure: { source: 'device', kind: 'public_ipv4_required' },
      },
      canAskHomeserver: false,
    })
    controller.askHomeserver(controller.cards.value[0].identity)
    expect(homeserver).not.toHaveBeenCalled()

    controller.configure(
      { setting: 'device+homeserver', device, homeserver },
      false,
    )
    // The failure was decided under the old setting, so it is decided again.
    expect(device).toHaveBeenCalledTimes(2)
    calls[1].reject(new LinkPreviewError('timeout'))
    await vi.advanceTimersByTimeAsync(0)
    expect(controller.cards.value[0].canAskHomeserver).toBe(true)
    controller.askHomeserver(controller.cards.value[0].identity)
    expect(homeserver).toHaveBeenCalledWith(A, expect.any(AbortSignal))
    await vi.advanceTimersByTimeAsync(0)
    expect(controller.cards.value[0].state).toEqual({
      status: 'ready',
      result: result('From the homeserver'),
    })
  })

  it('never asks the homeserver on its own', async () => {
    const { device, calls } = deviceSource()
    const homeserver = vi.fn(async () => result('x'))
    make({ setting: 'device+homeserver', device, homeserver })
    controller.edit(`${A} `, true)
    calls[0].reject(new LinkPreviewError('unavailable'))
    await vi.advanceTimersByTimeAsync(10_000)
    expect(homeserver).not.toHaveBeenCalled()
  })

  it('shows nothing for a lookup the plugin cancelled', async () => {
    const { device, calls } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`${A} `, true)
    calls[0].reject(new LinkPreviewError('cancelled'))
    await vi.advanceTimersByTimeAsync(0)
    expect(statuses()).toEqual([])
  })

  it('reports a page with nothing usable as no metadata', async () => {
    const { device, calls } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`${A} `, true)
    calls[0].resolve({ url: A, title: ' \u200B ', imageUnavailable: true })
    await vi.advanceTimersByTimeAsync(0)
    expect(controller.cards.value[0].state).toEqual({
      status: 'failed',
      failure: { source: 'device', kind: 'no_metadata' },
    })
  })
})

describe('setting and platform (§ 4)', () => {
  it('offers the homeserver in a browser, and asks it only on request', async () => {
    const homeserver = vi.fn(async () => result('x'))
    make({ setting: 'device+homeserver', homeserver })
    controller.edit(`${A} `, true)
    expect(controller.cards.value[0]).toMatchObject({
      state: { status: 'offer' },
      canAskHomeserver: true,
    })
    await vi.advanceTimersByTimeAsync(10_000)
    expect(homeserver).not.toHaveBeenCalled()
  })

  it('treats a stored device setting as off in a browser', () => {
    make({ setting: 'device', homeserver: vi.fn() })
    controller.edit(`${A} `, true)
    expect(statuses()).toEqual([])
  })

  it('makes no cards and no lookups when off', () => {
    const { device } = deviceSource()
    make({ setting: 'off', device })
    controller.edit(`${A} `, true)
    expect(statuses()).toEqual([])
    expect(device).not.toHaveBeenCalled()
  })

  it('cancels while suspended and comes back from the cache', async () => {
    const { device, calls } = deviceSource()
    const sources: PreviewSources = { setting: 'device', device }
    make(sources)
    controller.edit(`${A} ${B} `, true)
    calls[0].resolve(devicePreview(A))
    await vi.advanceTimersByTimeAsync(0)
    controller.configure(sources, true)
    expect(calls[1].signal.aborted).toBe(true)
    expect(statuses()).toEqual([])
    controller.configure(sources, false)
    expect(statuses()).toEqual([
      [A, 'ready'],
      [B, 'loading'],
    ])
    expect(device).toHaveBeenCalledTimes(3)
  })
})

describe('take', () => {
  it('hands the send its ready, kept cards in text order and starts afresh', async () => {
    const { device, calls } = deviceSource()
    make({ setting: 'device', device })
    controller.edit(`${B} ${A} ${C} `, true)
    calls[1].resolve(devicePreview(A))
    calls[0].resolve({
      ...devicePreview(B),
      image: {
        bytes: new Uint8Array([1]),
        mimeType: 'image/png',
        width: 1,
        height: 1,
      },
    })
    await vi.advanceTimersByTimeAsync(0)
    controller.discard(controller.cards.value[2].identity)
    const draft = controller.take()
    expect(draft.cards).toEqual([
      {
        matchedUrl: B,
        title: `Title ${B}`,
        description: null,
        siteName: null,
        image: { bytes: new Uint8Array([1]), mimeType: 'image/png' },
      },
      {
        matchedUrl: A,
        title: `Title ${A}`,
        description: null,
        siteName: null,
        image: null,
      },
    ])
    expect(draft.discarded).toEqual(new Set([C]))
    expect(statuses()).toEqual([])
  })
})

describe('fromDevice', () => {
  it('applies the composer field rule to every field', () => {
    expect(
      fromDevice({
        url: A,
        title: ' T ',
        description: 'd'.repeat(1001),
        siteName: '\u200BSite',
        imageUnavailable: true,
      }),
    ).toEqual({
      title: 'T',
      description: null,
      siteName: 'Site',
      image: null,
      imageLost: true,
    })
  })
  it('rejects a preview with nothing to show', () => {
    expect(() => fromDevice({ url: A, imageUnavailable: false })).toThrow(
      PreviewLookupError,
    )
  })
})

describe('previewFailureMessage (issue 1059)', () => {
  it('words the IPv4 failure for both a typo and an IPv6-only site', () => {
    const message = previewFailureMessage({
      source: 'device',
      kind: 'public_ipv4_required',
    })
    expect(message).toMatch(/No IPv4 address/)
    expect(message).toMatch(/Check the link/)
    expect(message).toMatch(/IPv6/)
  })
})

describe('homeserverLinkPreview', () => {
  const BASE_URL = 'http://axon.test'
  const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
  const UPLOAD = '22222222-2222-4222-8222-222222222222'
  const GENERATE = `${BASE_URL}/v1/accounts/${ACCOUNT}/link-previews/generate`
  const THUMBNAIL = `${BASE_URL}/v1/accounts/${ACCOUNT}/media/uploads/${UPLOAD}/preview`
  const server = setupServer()
  beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
  afterEach(() => server.resetHandlers())
  afterAll(() => server.close())
  beforeEach(() => vi.useRealTimers())

  const auth = {
    getToken: () => 'tok-test',
    onAuthFailure: () => {},
    LoginBootstrap: () => null,
  }
  const lookUp = homeserverLinkPreview(createApiClient(auth, BASE_URL), ACCOUNT)
  const ready = (imageUploadId: string | null) =>
    HttpResponse.json({
      data: {
        status: 'ready',
        thumbnail_status: imageUploadId === null ? 'absent' : 'ready',
        preview: {
          matched_url: A,
          title: ' Title ',
          description: '',
          image_upload_id: imageUploadId,
        },
      },
    })

  it('keeps the thumbnail as bytes and forgets its upload', async () => {
    let asked: unknown
    server.use(
      http.post(GENERATE, async ({ request }) => {
        asked = await request.json()
        return ready(UPLOAD)
      }),
      http.get(
        THUMBNAIL,
        () =>
          new HttpResponse(new Uint8Array([9, 8, 7]), {
            headers: { 'content-type': 'image/png' },
          }),
      ),
    )
    const preview = await lookUp(A, new AbortController().signal)
    expect(asked).toEqual({ url: A })
    expect(preview).toEqual({
      title: 'Title',
      description: null,
      siteName: null,
      image: { bytes: new Uint8Array([9, 8, 7]), mimeType: 'image/png' },
      imageLost: false,
    })
    expect(JSON.stringify(preview)).not.toContain(UPLOAD)
  })

  it('keeps the text and loses the image when the thumbnail cannot be read', async () => {
    server.use(
      http.post(GENERATE, () => ready(UPLOAD)),
      http.get(THUMBNAIL, () =>
        HttpResponse.json(
          { error: { code: 'not_found', message: 'gone' } },
          { status: 404 },
        ),
      ),
    )
    expect(await lookUp(A, new AbortController().signal)).toMatchObject({
      title: 'Title',
      image: null,
      imageLost: true,
    })
  })

  it('reports a status other than ready as its failure', async () => {
    server.use(
      http.post(GENERATE, () =>
        HttpResponse.json({
          data: { status: 'busy', thumbnail_status: 'absent' },
        }),
      ),
    )
    await expect(lookUp(A, new AbortController().signal)).rejects.toEqual(
      new PreviewLookupError({ source: 'homeserver', kind: 'busy' }),
    )
  })

  it('reports an error response as unavailable', async () => {
    server.use(
      http.post(GENERATE, () =>
        HttpResponse.json(
          { error: { code: 'bad_request', message: 'no' } },
          { status: 400 },
        ),
      ),
    )
    await expect(lookUp(A, new AbortController().signal)).rejects.toEqual(
      new PreviewLookupError({ source: 'homeserver', kind: 'unavailable' }),
    )
  })
})
