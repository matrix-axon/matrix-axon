import { describe, expect, it, vi } from 'vitest'
import type { ApiClient } from '../api/client'
import { createTimelineStoreCache } from '../stores/timeline-cache'
import type { MediaService } from '../media/media-service'
import { createIncomingShareStore } from './store'
import type { IncomingShare, IncomingSharePort } from './types'

const ACCOUNT = '00000000-0000-4000-8000-000000000001'
function fixture(
  state: IncomingShare['operation']['state'] = 'ready',
  accessToken?: () => Promise<string | null>,
) {
  const initial: IncomingShare = {
    version: 1,
    id: 'share',
    createdAt: 1,
    session: 'session',
    accountId: ACCOUNT,
    roomId: '!room:hs',
    text: 'https://example.org',
    files: [],
    operation: { id: 'op', state },
    importFailures: [],
    revision: 0,
  }
  let record = structuredClone(initial)
  let uploadFailure = false
  const calls: { action: string; args?: Record<string, unknown> }[] = []
  const port: IncomingSharePort = {
    subscribe: () => () => {},
    async call<T>(action: string, args?: Record<string, unknown>): Promise<T> {
      calls.push({ action, args: structuredClone(args) })
      if (action === 'list') return [structuredClone(record)] as T
      if (action === 'claim') {
        record.owner = 'owner'
        return structuredClone(record) as T
      }
      if (action === 'update') {
        const next = args?.manifest as IncomingShare
        if (
          next.session !== record.session ||
          next.revision !== record.revision
        )
          throw new Error('stale')
        record = { ...structuredClone(next), revision: next.revision + 1 }
        return structuredClone(record) as T
      }
      if (action === 'upload' && uploadFailure) throw { code: 'missingFile' }
      if (action === 'upload')
        return { uploadId: '00000000-0000-4000-8000-000000000002' } as T
      return {} as T
    },
  }
  const post = vi.fn().mockResolvedValue({
    data: { data: { event_id: '$confirmed' } },
    response: new Response(null, { status: 200 }),
  })
  const api = {
    POST: post,
    GET: vi.fn().mockResolvedValue({
      data: { data: [{ account_id: ACCOUNT, room_id: '!room:hs' }] },
    }),
  } as unknown as ApiClient
  const timelines = createTimelineStoreCache(api, {} as MediaService)
  const store = createIncomingShareStore(port, api, timelines, accessToken)
  return {
    store,
    calls,
    post,
    api,
    record: () => record,
    initial,
    seed(value: Partial<IncomingShare>) {
      Object.assign(record, structuredClone(value))
    },
    port,
    failUpload() {
      uploadFailure = true
    },
  }
}

describe('durable incoming-share recovery', () => {
  it('a delayed list cannot replace a newer editor revision', async () => {
    const f = fixture()
    await f.store.open('share')
    const snapshot = structuredClone(f.record())
    let finish!: (records: IncomingShare[]) => void
    const original = f.port.call
    f.port.call = async (action, args) => {
      if (action === 'list')
        return (await new Promise<IncomingShare[]>((resolve) => {
          finish = resolve
        })) as never
      return original(action, args)
    }
    const reading = f.store.refresh()
    await vi.waitFor(() => expect(finish).toBeDefined())
    await f.store.edit('newer text')
    await f.store.flushEdits()
    finish([snapshot])
    await reading
    expect(f.store.selected.value?.revision).toBe(f.record().revision)
    expect(f.store.editorText.value).toBe('newer text')
  })

  it('coalesces typing and flushes the latest text before Send', async () => {
    const f = fixture()
    await f.store.open('share')
    for (let i = 0; i < 200; i++) await f.store.edit(`caption ${i}`)
    expect(f.calls.filter((call) => call.action === 'update')).toHaveLength(0)
    expect(f.store.editorText.value).toBe('caption 199')
    await f.store.send()
    expect(f.post.mock.calls[0][1].body.body).toBe('caption 199')
    expect(f.calls.filter((call) => call.action === 'update')).toHaveLength(3)
  })
  it('flushes draft edits on close and invalidates delayed edits on reset', async () => {
    vi.useFakeTimers()
    try {
      const f = fixture()
      await f.store.open('share')
      await f.store.edit('kept draft')
      await f.store.close()
      expect(f.record().text).toBe('kept draft')
      await f.store.open('share')
      await f.store.edit('must not reappear')
      const before = f.calls.length
      f.store.reset()
      await vi.advanceTimersByTimeAsync(2000)
      expect(f.calls).toHaveLength(before)
    } finally {
      vi.useRealTimers()
    }
  })
  it('saves after idle and retains unsaved text across ownership expiry', async () => {
    vi.useFakeTimers()
    try {
      const f = fixture()
      await f.store.open('share')
      await f.store.edit('saved after idle')
      await vi.advanceTimersByTimeAsync(250)
      expect(f.record().text).toBe('saved after idle')
      await f.store.edit('unsaved on resume')
      f.seed({ owner: undefined })
      await f.store.refresh()
      expect(f.store.editorText.value).toBe('unsaved on resume')
      await f.store.flushEdits()
      expect(f.record().text).toBe('unsaved on resume')
    } finally {
      vi.useRealTimers()
    }
  })
  it('rejects a ninth preview before generation or retention', async () => {
    const f = fixture()
    f.seed({
      previews: Array.from({ length: 8 }, (_, i) => ({
        matched_url: `https://example.org/${i}`,
        provenance: 'source' as const,
      })),
    })
    await f.store.open('share')
    await f.store.generate('https://example.org/ninth')
    expect(f.post).not.toHaveBeenCalled()
    expect(f.calls.some((call) => call.action === 'retain')).toBe(false)
    expect(f.store.error.value).toContain('limit')
  })
  it('explains unavailable rooms and preserves HTTP drafts without a send', async () => {
    const f = fixture()
    await f.store.open('share')
    vi.mocked(f.api.GET).mockResolvedValueOnce({ data: { data: [] } } as never)
    await f.store.send()
    expect(f.store.error.value).toContain('room is no longer available')
    const original = f.port.call
    f.port.call = async (action, args) => {
      if (action === 'check_transport') throw { code: 'httpsRequired' }
      return original(action, args)
    }
    await f.store.send()
    expect(f.store.error.value).toContain('requires an HTTPS server')
    expect(f.post).not.toHaveBeenCalled()
    expect(f.store.selected.value).not.toBeNull()
  })

  it('explains an empty destination without recommending authentication', async () => {
    const f = fixture()
    f.seed({ roomId: '' })
    await f.store.open('share')
    await f.store.send()
    expect(f.store.error.value).toContain(
      'Choose a sending account and joined room',
    )
    expect(f.post).not.toHaveBeenCalled()
  })
  it('refresh reclaims expired ownership without sending', async () => {
    const f = fixture()
    await f.store.open('share')
    f.seed({ owner: undefined })
    await f.store.refresh()
    expect(f.calls.filter((call) => call.action === 'claim')).toHaveLength(2)
    expect(f.post).not.toHaveBeenCalled()
  })
  it('main-app session-only uploads pass credentials transiently without publishing them', async () => {
    const f = fixture('ready', async () => 'one-time-access')
    f.seed({
      previews: [
        {
          matched_url: 'https://example.org',
          provenance: 'source',
          thumbnail: 'thumb',
        },
      ],
    })
    await f.store.open('share')
    await f.store.send()
    expect(f.calls.find((call) => call.action === 'upload')?.args?.token).toBe(
      'one-time-access',
    )
    expect(f.calls.some((call) => call.action === 'publish')).toBe(false)
  })
  it('a missing retained image requires visible recovery and never regenerates automatically', async () => {
    const f = fixture()
    f.seed({
      previews: [
        {
          matched_url: 'https://example.org',
          provenance: 'generated',
          thumbnail: 'missing-thumb',
        },
      ],
    })
    await f.store.open('share')
    f.failUpload()
    await f.store.send()
    expect(f.store.error.value).toContain('missing or corrupt')
    expect(f.record().previews?.[0].thumbnail).toBe('missing-thumb')
    expect(f.post).not.toHaveBeenCalled()
    expect(f.calls.some((call) => call.action === 'ack')).toBe(false)
  })
  it('a generic proxy rejection is uncertain without the documented Axon error code', async () => {
    const f = fixture()
    f.post.mockResolvedValue({
      error: '<html>proxy error</html>',
      response: new Response(null, { status: 413 }),
    })
    await f.store.open('share')
    await f.store.send()
    expect(f.record().operation.state).toBe('unknown')
  })
  it('reuploads a retained thumbnail without regenerating the webpage', async () => {
    const f = fixture()
    f.seed({
      previews: [
        {
          matched_url: 'https://example.org',
          provenance: 'source',
          image_upload_id: 'expired',
          thumbnail: 'thumb',
        },
      ],
    })
    await f.store.open('share')
    await f.store.send()
    expect(f.calls.filter((call) => call.action === 'upload')).toHaveLength(1)
    expect(f.calls.find((call) => call.action === 'upload')?.args?.file).toBe(
      'thumb',
    )
    expect(f.post.mock.calls[0][0]).toBe(
      '/v1/accounts/{account_id}/rooms/{room_id}/send',
    )
    expect(f.post.mock.calls[0][1].body.url_previews[0].image_upload_id).toBe(
      '00000000-0000-4000-8000-000000000002',
    )
  })
  it('never repeats a confirmed first item or caption while resuming a batch', async () => {
    const f = fixture()
    f.seed({
      files: [
        {
          id: 'first',
          name: 'first.txt',
          mime: 'text/plain',
          bytes: 3,
          digest: 'fixture',
          thumbnail: false,
          operation: { id: 'first-op', state: 'sent', eventId: '$first' },
        },
        {
          id: 'second',
          name: 'second.txt',
          mime: 'text/plain',
          bytes: 3,
          digest: 'fixture',
          thumbnail: false,
          operation: { id: 'second-op', state: 'ready' },
        },
      ],
      previews: [
        {
          matched_url: 'https://example.org',
          provenance: 'source',
          thumbnail: 'already-delivered-thumb',
        },
      ],
    })
    await f.store.open('share')
    await f.store.send()
    expect(
      f.calls
        .filter((call) => call.action === 'upload')
        .map((call) => call.args?.file),
    ).toEqual(['second'])
    expect(f.post).toHaveBeenCalledTimes(1)
    expect(f.post.mock.calls[0][1].body.caption).toBe('')
    expect(f.post.mock.calls[0][1].body.url_previews).toBeUndefined()
    expect(f.record().files[0].operation?.eventId).toBe('$first')
  })
  it('navigation during a live dispatch does not release its ownership', async () => {
    const f = fixture()
    let finish!: (value: unknown) => void
    f.post.mockImplementation(
      () =>
        new Promise((resolve) => {
          finish = resolve
        }),
    )
    await f.store.open('share')
    const sending = f.store.send()
    await vi.waitFor(() => expect(f.post).toHaveBeenCalledTimes(1))
    await f.store.close()
    expect(f.store.selected.value?.owner).toBe('owner')
    expect(f.calls.some((call) => call.args?.release === true)).toBe(false)
    finish({
      data: { data: { event_id: '$confirmed' } },
      response: new Response(null, { status: 200 }),
    })
    await sending
    expect(f.record().operation.state).toBe('sent')
  })
  it('enumerates and claims without acknowledging or sending', async () => {
    const f = fixture()
    await f.store.refresh()
    await f.store.open('share')
    expect(f.calls.map((call) => call.action)).toEqual(['list', 'claim'])
    expect(f.post).not.toHaveBeenCalled()
  })
  it('persists dispatch before HTTP and event id before acknowledgment', async () => {
    const f = fixture()
    f.post.mockImplementation(async () => {
      expect(f.record().operation.state).toBe('dispatching')
      expect(f.calls.some((call) => call.action === 'ack')).toBe(false)
      return {
        data: { data: { event_id: '$confirmed' } },
        response: new Response(null, { status: 200 }),
      }
    })
    await f.store.open('share')
    await f.store.send()
    expect(f.record().operation).toMatchObject({
      state: 'sent',
      eventId: '$confirmed',
    })
    expect(f.calls.some((call) => call.action === 'ack')).toBe(true)
  })
  it('never automatically replays an uncertain dispatch', async () => {
    const f = fixture('unknown')
    await f.store.open('share')
    await f.store.send()
    await f.store.retry('op', false)
    expect(f.record().operation.state).toBe('unknown')
    expect(f.post).not.toHaveBeenCalled()
    await f.store.retry('op', true)
    await f.store.send()
    expect(f.post).toHaveBeenCalledTimes(1)
  })
  it('a lost success response stays unknown with no automatic retry', async () => {
    const f = fixture()
    f.post.mockRejectedValue(new TypeError('connection lost'))
    await f.store.open('share')
    await f.store.send()
    expect(f.record().operation.state).toBe('unknown')
    expect(f.calls.some((call) => call.action === 'ack')).toBe(false)
    await f.store.send()
    expect(f.post).toHaveBeenCalledTimes(1)
  })
  it('explicit size rejection is a definite failure and retains the draft', async () => {
    const f = fixture()
    f.post.mockResolvedValue({
      error: { error: { code: 'event_too_large' } },
      response: new Response(null, { status: 413 }),
    })
    await f.store.open('share')
    await f.store.send()
    expect(f.record().operation.state).toBe('failed')
    expect(f.calls.some((call) => call.action === 'ack')).toBe(false)
  })
  it('changing a matched URL invalidates only its card', async () => {
    const f = fixture()
    f.initial.previews = []
    await f.store.open('share')
    f.store.selected.value!.previews = [
      {
        matched_url: 'https://example.org',
        title: 'Title',
        provenance: 'source',
      },
    ]
    await f.store.edit('Some text https://example.org')
    await f.store.flushEdits()
    expect(f.record().previews).toHaveLength(1)
    await f.store.edit('https://example.org/new')
    await f.store.flushEdits()
    expect(f.record().previews).toEqual([])
  })
  it('a stale enumeration cannot repopulate signed-out content', async () => {
    const f = fixture()
    let resolve!: (value: IncomingShare[]) => void
    const port: IncomingSharePort = {
      subscribe: () => () => {},
      call: () =>
        new Promise((done) => {
          resolve = done as typeof resolve
        }),
    }
    const store = createIncomingShareStore(
      port,
      f.api,
      createTimelineStoreCache(f.api, {} as MediaService),
    )
    const reading = store.refresh()
    await Promise.resolve()
    store.reset()
    resolve([f.initial])
    await reading
    expect(store.pending.value).toEqual([])
  })
})
