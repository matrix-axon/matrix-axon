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
import type { AuthProvider } from '../auth/provider'
import {
  createMediaService,
  MAX_UPLOAD_BYTES,
  uploadFailureMessage,
  type MediaFailure,
} from './media-service'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '11111111-1111-4111-8111-111111111111'
const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47])

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function stubAuth(token: string | null = 'tok'): AuthProvider & {
  failures: number
} {
  const auth = {
    failures: 0,
    getToken: () => token,
    onAuthFailure() {
      auth.failures += 1
    },
    LoginBootstrap: () => null,
  }
  return auth
}

/** Serve raw PNG bytes for every media path, counting how often it is hit. */
function serveBytes(): { fetches: number } {
  const state = { fetches: 0 }
  server.use(
    http.get(`${BASE_URL}/v1/media/:account/:server/:media`, () => {
      state.fetches += 1
      return new HttpResponse(PNG, {
        headers: { 'content-type': 'image/png' },
      })
    }),
  )
  return state
}

let created: string[]
let revoked: string[]

beforeEach(() => {
  created = []
  revoked = []
  let n = 0
  vi.spyOn(URL, 'createObjectURL').mockImplementation(() => {
    const url = `blob:${n++}`
    created.push(url)
    return url
  })
  vi.spyOn(URL, 'revokeObjectURL').mockImplementation((url) => {
    revoked.push(String(url))
  })
})

afterEach(() => vi.restoreAllMocks())

describe('createMediaService.acquire', () => {
  it('resolves an mxc uri to an object url with a bearer token', async () => {
    let seenAuth: string | null = null
    server.use(
      http.get(
        `${BASE_URL}/v1/media/:account/:server/:media`,
        ({ request }) => {
          seenAuth = request.headers.get('authorization')
          return new HttpResponse(PNG, {
            headers: { 'content-type': 'image/png' },
          })
        },
      ),
    )
    const media = createMediaService({
      auth: stubAuth('tok-x'),
      baseUrl: BASE_URL,
    })
    const handle = await media.acquire(ACCOUNT, 'mxc://hs/abc')

    expect(seenAuth).toBe('Bearer tok-x')
    expect(handle.result).toEqual({ ok: true, url: 'blob:0' })
  })

  it('shares one fetch and one object url across concurrent acquires', async () => {
    const state = serveBytes()
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })

    const [a, b] = await Promise.all([
      media.acquire(ACCOUNT, 'mxc://hs/dup'),
      media.acquire(ACCOUNT, 'mxc://hs/dup'),
    ])

    expect(state.fetches).toBe(1)
    expect(created).toHaveLength(1)
    expect(a.result).toEqual({ ok: true, url: 'blob:0' })
    expect(b.result).toEqual({ ok: true, url: 'blob:0' })
  })

  it('resolves a generated thumbnail through the thumbnail route', async () => {
    let seenUrl: string | null = null
    let seenAuth: string | null = null
    server.use(
      http.get(
        `${BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
        ({ request }) => {
          seenUrl = request.url
          seenAuth = request.headers.get('authorization')
          return new HttpResponse(PNG, {
            headers: { 'content-type': 'image/png' },
          })
        },
      ),
    )
    const media = createMediaService({
      auth: stubAuth('tok-thumb'),
      baseUrl: BASE_URL,
    })

    const handle = await media.acquire(ACCOUNT, 'mxc://hs/full', {
      thumbnail: { width: 320, height: 320, method: 'scale' },
    })

    expect(handle.result).toEqual({ ok: true, url: 'blob:0' })
    expect(seenAuth).toBe('Bearer tok-thumb')
    expect(seenUrl).toBe(
      `${BASE_URL}/v1/media/${ACCOUNT}/hs/full/thumbnail?width=320&height=320&method=scale`,
    )
  })

  it('caches generated thumbnails separately from the full media object', async () => {
    let fullFetches = 0
    let thumbnailFetches = 0
    server.use(
      http.get(`${BASE_URL}/v1/media/:account/:server/:media`, () => {
        fullFetches += 1
        return new HttpResponse(PNG, {
          headers: { 'content-type': 'image/png' },
        })
      }),
      http.get(`${BASE_URL}/v1/media/:account/:server/:media/thumbnail`, () => {
        thumbnailFetches += 1
        return new HttpResponse(PNG, {
          headers: { 'content-type': 'image/png' },
        })
      }),
    )
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })

    const full = await media.acquire(ACCOUNT, 'mxc://hs/same')
    const thumb = await media.acquire(ACCOUNT, 'mxc://hs/same', {
      thumbnail: { width: 320, height: 320 },
    })
    full.release()
    thumb.release()
    const thumbAgain = await media.acquire(ACCOUNT, 'mxc://hs/same', {
      thumbnail: { width: 320, height: 320 },
    })

    expect(fullFetches).toBe(1)
    expect(thumbnailFetches).toBe(1)
    expect(created).toEqual(['blob:0', 'blob:1'])
    expect(thumbAgain.result).toEqual({ ok: true, url: 'blob:1' })
  })

  it('keeps a released blob available below the cap', async () => {
    serveBytes()
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })

    const handle = await media.acquire(ACCOUNT, 'mxc://hs/keep')
    handle.release()
    expect(revoked).toEqual([])

    // Re-acquiring hits the cache: no second fetch, no new object url.
    const again = await media.acquire(ACCOUNT, 'mxc://hs/keep')
    expect(again.result).toEqual({ ok: true, url: 'blob:0' })
    expect(created).toHaveLength(1)
  })

  it('revokes the oldest zero-ref blob once the cap is exceeded', async () => {
    serveBytes()
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })

    // 33 distinct keys, each acquired then released → 33 zero-ref entries,
    // one past the cap of 32, so the oldest (blob:0) is revoked exactly once.
    for (let i = 0; i < 33; i++) {
      const handle = await media.acquire(ACCOUNT, `mxc://hs/m${i}`)
      handle.release()
    }

    expect(revoked).toEqual(['blob:0'])
  })

  it('never revokes an entry that still has holders, even past the cap', async () => {
    serveBytes()
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })

    // Hold one entry (refs stays 1, never zero-ref), then churn far past the
    // cap. The held blob must never be revoked — the core hazard.
    const held = await media.acquire(ACCOUNT, 'mxc://hs/held')
    expect(held.result).toEqual({ ok: true, url: 'blob:0' })

    for (let i = 0; i < 40; i++) {
      const handle = await media.acquire(ACCOUNT, `mxc://hs/n${i}`)
      handle.release()
    }

    expect(revoked).not.toContain('blob:0')
    held.release()
  })

  it('maps a 401 to an auth failure and notifies the provider', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/media/:account/:server/:media`, () =>
        HttpResponse.json({ error: {} }, { status: 401 }),
      ),
    )
    const auth = stubAuth()
    const media = createMediaService({ auth, baseUrl: BASE_URL })

    const handle = await media.acquire(ACCOUNT, 'mxc://hs/x')
    expect(handle.result).toEqual({
      ok: false,
      error: { kind: 'auth', status: 401 },
    })
    expect(auth.failures).toBe(1)
  })

  it('maps 404 and 413 to their failure kinds', async () => {
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })

    server.use(
      http.get(`${BASE_URL}/v1/media/:account/:server/:media`, () =>
        HttpResponse.json({ error: {} }, { status: 404 }),
      ),
    )
    expect((await media.acquire(ACCOUNT, 'mxc://hs/a')).result).toEqual({
      ok: false,
      error: { kind: 'not_found', status: 404 },
    })

    server.use(
      http.get(`${BASE_URL}/v1/media/:account/:server/:media`, () =>
        HttpResponse.json({ error: {} }, { status: 413 }),
      ),
    )
    expect((await media.acquire(ACCOUNT, 'mxc://hs/b')).result).toEqual({
      ok: false,
      error: { kind: 'too_large', status: 413 },
    })
  })

  it('returns a network failure for a malformed mxc uri without fetching', async () => {
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })
    const handle = await media.acquire(ACCOUNT, 'not-an-mxc')
    expect(handle.result).toEqual({ ok: false, error: { kind: 'network' } })
  })
})

describe('createMediaService.fetchBlobUrl', () => {
  it('downloads a fresh, uncached object url the caller owns', async () => {
    serveBytes()
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })

    const first = await media.fetchBlobUrl(ACCOUNT, 'mxc://hs/dl')
    const second = await media.fetchBlobUrl(ACCOUNT, 'mxc://hs/dl')

    // Not cached: each call downloads and mints its own url.
    expect(first).toEqual({ ok: true, url: 'blob:0' })
    expect(second).toEqual({ ok: true, url: 'blob:1' })
  })
})

describe('createMediaService.upload', () => {
  const UPLOAD_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/media/uploads`
  const UPLOAD_ID = '22222222-2222-4222-8222-222222222222'

  /**
   * Capture what the client put on the wire.
   *
   * Deliberately not asserting the *body* bytes here: under vitest's jsdom
   * environment a jsdom `File` is not undici's `Blob`, so undici stringifies it
   * and the request body arrives as the literal `"undefined"` — an artifact of
   * the test environment, not of the client (the `content-type` below still
   * comes through, proving the Blob was recognized as one). Passing a `File` as
   * a `fetch` body is the spec'd browser path, so the bytes are asserted where
   * a real browser runs them: `e2e/media-send.spec.ts`.
   */
  function captureUpload(): {
    kind?: string | null
    filename?: string | null
    contentType?: string | null
  } {
    const seen: {
      kind?: string | null
      filename?: string | null
      contentType?: string | null
    } = {}
    server.use(
      http.post(UPLOAD_PATH, ({ request }) => {
        const url = new URL(request.url)
        seen.kind = url.searchParams.get('kind')
        seen.filename = url.searchParams.get('filename')
        seen.contentType = request.headers.get('content-type')
        return HttpResponse.json({ data: { upload_id: UPLOAD_ID } })
      }),
    )
    return seen
  }

  it('stages an image as kind=image with the browser-determined mime', async () => {
    const seen = captureUpload()
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })

    const result = await media.upload(
      ACCOUNT,
      new File(['bytes'], 'cat photo.png', { type: 'image/png' }),
    )

    expect(result).toEqual({ ok: true, uploadId: UPLOAD_ID })
    expect(seen.kind).toBe('image')
    expect(seen.filename).toBe('cat photo.png')
    // The server rejects kind=image without an image/* type, so these two must
    // agree — which they do by construction, both coming from `File.type`.
    expect(seen.contentType).toBe('image/png')
  })

  it('stages a non-image as kind=file', async () => {
    const seen = captureUpload()
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })

    await media.upload(
      ACCOUNT,
      new File(['%PDF'], 'notes.pdf', { type: 'application/pdf' }),
    )

    expect(seen.kind).toBe('file')
    expect(seen.contentType).toBe('application/pdf')
  })

  it('stages a file the browser could not type as kind=file', async () => {
    const seen = captureUpload()
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })

    await media.upload(ACCOUNT, new File(['x'], 'mystery.bin', { type: '' }))

    // An empty File.type must not become kind=image, and must not send a
    // bogus content-type — the server treats the header as optional.
    expect(seen.kind).toBe('file')
    expect(seen.contentType).toBeNull()
  })

  it('rejects an oversized file without making a request', async () => {
    // No msw handler registered: a request here would fail the suite outright
    // (`onUnhandledRequest: 'error'`), which is exactly the assertion.
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })
    const huge = new File(['x'], 'huge.bin')
    Object.defineProperty(huge, 'size', { value: MAX_UPLOAD_BYTES + 1 })

    const result = await media.upload(ACCOUNT, huge)

    expect(result).toEqual({ ok: false, error: { kind: 'too_large' } })
  })

  it('surfaces the server message when the upload is rejected', async () => {
    server.use(
      http.post(UPLOAD_PATH, () =>
        HttpResponse.json(
          {
            error: {
              code: 'bad_request',
              message: 'filename must not be empty',
            },
          },
          { status: 400 },
        ),
      ),
    )
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })

    const result = await media.upload(ACCOUNT, new File(['x'], 'x.bin'))

    expect(result).toEqual({
      ok: false,
      error: {
        kind: 'rejected',
        status: 400,
        message: 'filename must not be empty',
      },
    })
    expect(
      uploadFailureMessage((result as { error: MediaFailure }).error),
    ).toBe('filename must not be empty')
  })

  it('maps a 413 to too_large, preferring the server cap over ours', async () => {
    server.use(
      http.post(UPLOAD_PATH, () =>
        HttpResponse.json(
          {
            error: {
              code: 'payload_too_large',
              message: 'upload exceeds configured limit of 1024 bytes',
            },
          },
          { status: 413 },
        ),
      ),
    )
    const media = createMediaService({ auth: stubAuth(), baseUrl: BASE_URL })

    const result = await media.upload(ACCOUNT, new File(['x'], 'x.bin'))

    expect(result).toMatchObject({ ok: false, error: { kind: 'too_large' } })
    expect(
      uploadFailureMessage((result as { error: MediaFailure }).error),
    ).toBe('upload exceeds configured limit of 1024 bytes')
  })

  it('reports a 401 to the auth provider, as the download path does', async () => {
    server.use(
      http.post(UPLOAD_PATH, () =>
        HttpResponse.json(
          { error: { code: 'unauthorized', message: 'token revoked' } },
          { status: 401 },
        ),
      ),
    )
    const auth = stubAuth()
    const media = createMediaService({ auth, baseUrl: BASE_URL })

    const result = await media.upload(ACCOUNT, new File(['x'], 'x.bin'))

    expect(result).toMatchObject({ ok: false, error: { kind: 'auth' } })
    expect(auth.failures).toBe(1)
  })
})

describe('the transport seam (ADR 0102 § 2)', () => {
  /**
   * Both media paths default to `browserPlatform()`, so msw would keep every
   * other test in this file green even if the injected platform stopped being
   * threaded through. These pin download and upload separately, because they
   * are two distinct `fetch` call sites.
   */
  it('downloads through the injected fetch', async () => {
    const calls: string[] = []
    const injected: typeof globalThis.fetch = async (input) => {
      calls.push(String(input instanceof Request ? input.url : input))
      return new Response(new Blob(['bytes']), { status: 200 })
    }

    const media = createMediaService({
      auth: stubAuth('tok-dl'),
      baseUrl: BASE_URL,
      platform: { fetch: injected },
    })
    const handle = await media.acquire(ACCOUNT, 'mxc://hs/abc')

    expect(calls).toHaveLength(1)
    expect(calls[0]).toContain('/v1/media/')
    expect(handle.result.ok).toBe(true)
  })

  it('uploads through the injected fetch', async () => {
    const methods: (string | undefined)[] = []
    const injected: typeof globalThis.fetch = async (input, init) => {
      methods.push(
        input instanceof Request ? input.method : (init?.method ?? 'GET'),
      )
      return new Response(JSON.stringify({ data: { upload_id: 'u1' } }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      })
    }

    const media = createMediaService({
      auth: stubAuth('tok-up'),
      baseUrl: BASE_URL,
      platform: { fetch: injected },
    })
    await media.upload(ACCOUNT, new File(['x'], 'x.png', { type: 'image/png' }))

    expect(methods).toEqual(['POST'])
  })
})
