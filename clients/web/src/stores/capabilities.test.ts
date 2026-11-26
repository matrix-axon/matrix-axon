import { signal } from '@preact/signals'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createApiClient } from '../api/client'
import { connectCapabilities, createServerCapabilities } from './capabilities'

const BASE_URL = 'http://axon.test'
const CAPABILITIES = `${BASE_URL}/v1/capabilities`

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function make() {
  const api = createApiClient(
    {
      getToken: () => 'tok',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
  return createServerCapabilities(api)
}

/** Serve `answer`, counting requests. */
function serve(answer: () => Response) {
  let asked = 0
  server.use(
    http.get(CAPABILITIES, () => {
      asked += 1
      return answer()
    }),
  )
  return () => asked
}

const settled = () => new Promise((resolve) => setTimeout(resolve, 20))

describe('server capabilities (ADR 0132 § 8)', () => {
  it('reads edit_url_previews and asks once', async () => {
    const asked = serve(() =>
      HttpResponse.json({ data: { edit_url_previews: true } }),
    )
    const capabilities = make()
    expect(capabilities.editUrlPreviews.value).toBeNull()
    capabilities.refresh()
    capabilities.refresh()
    await settled()
    expect(capabilities.editUrlPreviews.value).toBe(true)
    capabilities.refresh()
    await settled()
    expect(asked()).toBe(1)
  })

  it('keeps an older server’s 404 as no for the session', async () => {
    const asked = serve(() =>
      HttpResponse.json({ error: { code: 'not_found' } }, { status: 404 }),
    )
    const capabilities = make()
    capabilities.refresh()
    await settled()
    expect(capabilities.editUrlPreviews.value).toBe(false)
    capabilities.refresh()
    await settled()
    expect(asked()).toBe(1)
  })

  it('treats any other failure as not known, and asks again', async () => {
    let status = 503
    const asked = serve(() =>
      status === 503
        ? HttpResponse.json({ error: { code: 'x' } }, { status })
        : HttpResponse.json({ data: { edit_url_previews: true } }),
    )
    const capabilities = make()
    capabilities.refresh()
    await settled()
    expect(capabilities.editUrlPreviews.value).toBeNull()
    status = 200
    capabilities.refresh()
    await settled()
    expect(capabilities.editUrlPreviews.value).toBe(true)
    expect(asked()).toBe(2)
  })

  it('reads the media kinds and send-media fields (ADR 0142 § 1)', async () => {
    serve(() =>
      HttpResponse.json({
        data: {
          edit_url_previews: true,
          media_upload_kinds: ['image', 'file', 'video', 'audio'],
          send_media_thumbnails: true,
        },
      }),
    )
    const capabilities = make()
    expect(capabilities.mediaUploadKinds.value).toBeNull()
    expect(capabilities.sendMediaThumbnails.value).toBeNull()
    capabilities.refresh()
    await settled()
    expect(capabilities.mediaUploadKinds.value).toEqual([
      'image',
      'file',
      'video',
      'audio',
    ])
    expect(capabilities.sendMediaThumbnails.value).toBe(true)
    capabilities.reset()
    expect(capabilities.mediaUploadKinds.value).toBeNull()
    expect(capabilities.sendMediaThumbnails.value).toBeNull()
  })

  it('reads a server without the media fields as image and file only', async () => {
    let route = true
    serve(() =>
      route
        ? HttpResponse.json({ data: { edit_url_previews: true } })
        : HttpResponse.json({ error: { code: 'not_found' } }, { status: 404 }),
    )
    const capabilities = make()
    capabilities.refresh()
    await settled()
    expect(capabilities.mediaUploadKinds.value).toEqual(['image', 'file'])
    expect(capabilities.sendMediaThumbnails.value).toBe(false)
    // A server with no route at all supports neither.
    route = false
    capabilities.recheck()
    await settled()
    expect(capabilities.mediaUploadKinds.value).toEqual(['image', 'file'])
    expect(capabilities.sendMediaThumbnails.value).toBe(false)
    expect(capabilities.editUrlPreviews.value).toBe(false)
  })

  it('asks when signed in, and forgets at sign-out', async () => {
    serve(() => HttpResponse.json({ data: { edit_url_previews: true } }))
    const signedIn = signal(false)
    const capabilities = make()
    const stop = connectCapabilities(signedIn, capabilities)
    await settled()
    expect(capabilities.editUrlPreviews.value).toBeNull()
    signedIn.value = true
    await settled()
    expect(capabilities.editUrlPreviews.value).toBe(true)
    signedIn.value = false
    expect(capabilities.editUrlPreviews.value).toBeNull()
    stop()
  })

  it('asks again on a reconnect, which is how a server upgrade shows', async () => {
    let status = 404
    const asked = serve(() =>
      status === 404
        ? HttpResponse.json({ error: { code: 'not_found' } }, { status })
        : status === 503
          ? HttpResponse.json({ error: { code: 'x' } }, { status })
          : HttpResponse.json({ data: { edit_url_previews: true } }),
    )
    const signedIn = signal(true)
    const reconnects = signal(0)
    const capabilities = make()
    const stop = connectCapabilities(signedIn, capabilities, reconnects)
    await settled()
    expect(capabilities.editUrlPreviews.value).toBe(false)
    // An older server's no is kept until the socket comes back...
    capabilities.refresh()
    await settled()
    expect(asked()).toBe(1)
    // ...and a failed re-check leaves it standing.
    status = 503
    reconnects.value = 1
    await settled()
    expect(capabilities.editUrlPreviews.value).toBe(false)
    status = 200
    reconnects.value = 2
    await settled()
    expect(capabilities.editUrlPreviews.value).toBe(true)
    expect(asked()).toBe(3)
    // Signed out, a reconnect asks nothing.
    signedIn.value = false
    reconnects.value = 3
    await settled()
    expect(asked()).toBe(3)
    stop()
  })
})
