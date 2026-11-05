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
      if (action === 'update' || action === 'remove_file') {
        const next = args?.manifest as IncomingShare
        if (
          next.session !== record.session ||
          next.revision !== record.revision
        )
          throw new Error('stale')
        record = { ...structuredClone(next), revision: next.revision + 1 }
        if (action === 'remove_file')
          record.files = record.files.filter((file) => file.id !== args?.file)
        return structuredClone(record) as T
      }
      if (action === 'upload' && uploadFailure) throw { code: 'missingFile' }
      if (action === 'device_preview') {
        record.previews = [
          {
            matched_url: args?.url as string,
            title: 'Device preview',
            provenance: 'generated',
          },
        ]
        record.revision += 1
        return structuredClone(record) as T
      }
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
  it('keeps credential publication failures separate from successful inbox reads', async () => {
    const f = fixture()
    f.store.sessionError.value = 'Sharing credentials are unavailable.'
    await f.store.refresh()
    expect(f.store.unavailable.value).toBe(false)
    expect(f.store.sessionError.value).toBe(
      'Sharing credentials are unavailable.',
    )
  })
  it('cancels and joins device preview work before releasing a closing draft', async () => {
    const f = fixture()
    await f.store.open('share')
    const original = f.port.call
    let complete!: () => void
    f.port.call = async (action, args) => {
      if (action === 'device_preview') {
        await new Promise<void>((resolve) => {
          complete = resolve
        })
        throw { code: 'cancelled' }
      }
      if (action === 'cancel_preview') {
        complete()
        return {} as never
      }
      return original(action, args)
    }
    const generating = f.store.generate('https://example.org')
    await vi.waitFor(() => expect(complete).toBeDefined())
    await f.store.close()
    await generating
    expect(f.store.selected.value).toBeNull()
    expect(
      f.calls.some(
        (call) => call.action === 'update' && call.args?.release === true,
      ),
    ).toBe(true)
    expect(f.store.busy.value).toBe(false)
  })
  it('explains unsupported preview URLs without marking the text draft invalid', async () => {
    const f = fixture()
    f.seed({ text: 'https://user:password@example.org/article' })
    await f.store.open('share')
    await f.store.generate(f.record().text)
    expect(f.store.error.value).toContain('original link remains sendable')
    expect(f.calls.some((call) => call.action === 'device_preview')).toBe(false)
  })
  it('joins a preview closed before its native request starts', async () => {
    const f = fixture()
    await f.store.open('share')
    const generating = f.store.generate('https://example.org')
    await f.store.close()
    await generating
    expect(f.store.selected.value).toBeNull()
    expect(f.calls.some((call) => call.action === 'device_preview')).toBe(false)
  })
  it('joins a late successful preview before releasing its new revision', async () => {
    const f = fixture()
    await f.store.open('share')
    const original = f.port.call
    let finish!: () => void
    f.port.call = async (action, args) => {
      if (action === 'device_preview')
        await new Promise<void>((resolve) => {
          finish = resolve
        })
      if (action === 'cancel_preview') {
        finish()
        return {} as never
      }
      return original(action, args)
    }
    const generating = f.store.generate('https://example.org')
    await vi.waitFor(() => expect(finish).toBeDefined())
    await f.store.close()
    await generating
    expect(f.store.selected.value).toBeNull()
    expect(
      f.calls.filter((call) => call.action === 'update').at(-1)?.args?.release,
    ).toBe(true)
  })
  function inboxFixture(
    states: IncomingShare['operation']['state'][] = [
      'ready',
      'unknown',
      'sent',
    ],
  ) {
    const f = fixture()
    const records = states.map((state, index) => ({
      ...structuredClone(f.initial),
      id: `share-${index}`,
      operation: { id: `operation-${index}`, state },
    }))
    const port: IncomingSharePort = {
      subscribe: () => () => {},
      call: vi.fn(async (action, args) => {
        if (action === 'list') return structuredClone(records) as never
        const index = records.findIndex((record) => record.id === args?.id)
        if (index < 0) throw { code: 'stale' }
        if (action === 'claim') {
          records[index].owner = 'review-owner'
          return structuredClone(records[index]) as never
        }
        if (action === 'discard') {
          if (args?.owner !== undefined && records[index].owner !== args.owner)
            throw { code: 'stale' }
          if (
            records[index].owner &&
            records[index].operation.state === 'dispatching'
          )
            throw { code: 'sending' }
          records.splice(index, 1)
          return {} as never
        }
        throw new Error('unsupported fixture action')
      }),
    }
    const store = createIncomingShareStore(
      port,
      f.api,
      createTimelineStoreCache(f.api, {} as MediaService),
    )
    return { store, port, records, post: f.post }
  }

  it('clears the captured inbox including the reviewed share without sending', async () => {
    const f = inboxFixture()
    await f.store.refresh()
    await f.store.open('share-0')
    await f.store.discardAll()
    expect(f.records).toEqual([])
    expect(f.store.pending.value).toEqual([])
    expect(f.store.selected.value).toBeNull()
    expect(f.store.notice.value).toBe('Cleared 3 pending shares.')
    expect(f.post).not.toHaveBeenCalled()
  })

  it('keeps in-flight sends and continues clearing the rest after a failure', async () => {
    const f = inboxFixture()
    f.records[1].owner = 'extension-owner'
    f.records[1].operation.state = 'dispatching'
    await f.store.refresh()
    await f.store.discardAll()
    expect(f.records.map((record) => record.id)).toEqual(['share-1'])
    expect(f.store.pending.value.map((record) => record.id)).toEqual([
      'share-1',
    ])
    expect(f.store.error.value).toContain('1 share could not be cleared')
    expect(f.store.notice.value).toBe('Cleared 2 pending shares.')
    expect(f.post).not.toHaveBeenCalled()
  })

  it.each(['stale', 'unavailable', 'authentication'])(
    'reports a %s discard failure without claiming another composer owns it',
    async (code) => {
      const f = inboxFixture()
      const original = f.port.call
      f.port.call = vi.fn(async (action, args) => {
        if (action === 'discard' && args?.id === 'share-1') throw { code }
        return (await original(action, args)) as never
      })
      await f.store.refresh()
      await f.store.discardAll()
      expect(f.records.map((record) => record.id)).toEqual(['share-1'])
      expect(f.store.error.value).toContain('1 share could not be cleared')
      expect(f.store.error.value).not.toContain('open elsewhere')
      expect(f.store.error.value).not.toContain('five minutes')
      expect(f.store.notice.value).toBe('Cleared 2 pending shares.')
    },
  )

  it('keeps busy and inaccessible shares and reports both reasons', async () => {
    const f = inboxFixture()
    f.records[1].owner = 'extension-owner'
    f.records[1].operation.state = 'dispatching'
    const original = f.port.call
    f.port.call = vi.fn(async (action, args) => {
      if (action === 'discard' && args?.id === 'share-1') throw { code: 'busy' }
      if (action === 'discard' && args?.id === 'share-2')
        throw { code: 'authentication' }
      return (await original(action, args)) as never
    })
    await f.store.refresh()
    await f.store.discardAll()
    expect(f.records.map((record) => record.id)).toEqual(['share-1', 'share-2'])
    expect(f.store.notice.value).toBe('Cleared 1 pending share.')
    expect(f.store.error.value).toContain('2 shares could not be cleared')
    expect(f.store.error.value).toContain('sign in or refresh your session')
    expect(f.store.error.value).toContain('Saved shares are temporarily busy')
    expect(f.post).not.toHaveBeenCalled()
  })

  it('retains every mixed clear failure reason even when refresh clears its previous error', async () => {
    const f = inboxFixture()
    const original = f.port.call
    f.port.call = vi.fn(async (action, args) => {
      if (action === 'discard') {
        const code = {
          'share-0': 'busy',
          'share-1': 'sending',
          'share-2': 'authentication',
        }[args?.id as string]
        throw { code }
      }
      return (await original(action, args)) as never
    })
    await f.store.refresh()
    f.store.unavailable.value = true
    f.store.error.value = 'Older inbox read failure'
    await f.store.discardAll()
    expect(f.store.error.value).toContain('3 shares could not be cleared')
    expect(f.store.error.value).toContain('Saved shares are temporarily busy')
    expect(f.store.error.value).toContain('A share is still sending')
    expect(f.store.error.value).toContain('sign in or refresh your session')
    expect(f.store.error.value).not.toContain('null')
    expect(f.store.error.value).not.toContain('Older inbox read failure')
    expect(f.store.notice.value).toBe('Cleared 0 pending shares.')
  })

  it('cancels only the current device request and allows a fresh request after close', async () => {
    const f = fixture()
    await f.store.open('share')
    const original = f.port.call
    let complete!: () => void
    const cancelled: string[] = []
    f.port.call = async (action, args) => {
      if (action === 'device_preview' && complete === undefined) {
        await new Promise<void>((resolve) => {
          complete = resolve
        })
        throw { code: 'cancelled' }
      }
      if (action === 'cancel_preview') {
        cancelled.push(args?.requestId as string)
        complete()
        return {} as never
      }
      return original(action, args)
    }
    const first = f.store.generate('https://example.org')
    await vi.waitFor(() => expect(complete).toBeDefined())
    const closing = f.store.close()
    await f.store.generate('https://example.org') // Rejected while busy; cannot reset cancellation.
    await closing
    await first
    expect(cancelled).toHaveLength(1)
    await f.store.open('share')
    await f.store.generate('https://example.org')
    expect(f.store.selected.value?.previews?.[0]?.title).toBe('Device preview')
    expect(f.store.error.value).toBeNull()
    expect(
      f.calls.filter((call) => call.action === 'device_preview')[0]?.args
        ?.requestId,
    ).not.toBe(cancelled[0])
  })

  it('clears seven idle composers without waiting for their leases or adopting their owners', async () => {
    const f = inboxFixture(Array(7).fill('ready'))
    for (const record of f.records) record.owner = 'retained-composer'
    await f.store.refresh()
    await f.store.discardAll()
    expect(f.records).toEqual([])
    expect(f.store.error.value).toBeNull()
    expect(f.store.notice.value).toBe('Cleared 7 pending shares.')
    expect(
      vi
        .mocked(f.port.call)
        .mock.calls.filter(([action]) => action === 'discard')
        .every(([, args]) => args?.owner === undefined),
    ).toBe(true)
    expect(f.post).not.toHaveBeenCalled()
  })

  it('clears an idle reviewed draft after its previous claim has been replaced', async () => {
    const f = inboxFixture(['ready'])
    await f.store.refresh()
    await f.store.open('share-0')
    f.records[0].owner = 'replacement-review-owner'
    await f.store.discardAll()
    expect(f.records).toEqual([])
    expect(f.store.error.value).toBeNull()
  })

  it('does not clear shares arriving after the confirmed selection', async () => {
    const f = inboxFixture(['ready'])
    await f.store.refresh()
    const ids = f.store.pending.value.map((record) => record.id)
    f.records.push({ ...structuredClone(f.records[0]), id: 'new-share' })
    await f.store.discardAll(ids)
    expect(f.records.map((record) => record.id)).toEqual(['new-share'])
    expect(f.store.pending.value.map((record) => record.id)).toEqual([
      'new-share',
    ])
  })

  it('stops bulk deletion and ignores its completion after session reset', async () => {
    const f = inboxFixture()
    await f.store.refresh()
    const original = f.port.call
    let release!: () => void
    f.port.call = vi.fn(async (action, args) => {
      if (action === 'discard')
        await new Promise<void>((resolve) => {
          release = resolve
        })
      return (await original(action, args)) as never
    })
    const clearing = f.store.discardAll()
    await vi.waitFor(() => expect(release).toBeDefined())
    f.store.reset()
    release()
    await clearing
    expect(
      vi
        .mocked(f.port.call)
        .mock.calls.filter(([action]) => action === 'discard'),
    ).toHaveLength(1)
    expect(f.store.pending.value).toEqual([])
    expect(f.store.notice.value).toBeNull()
    expect(f.store.error.value).toBeNull()
  })

  it('distinguishes an unreadable inbox from an empty inbox and recovers', async () => {
    const f = inboxFixture([])
    const original = f.port.call
    f.port.call = async () => {
      throw { code: 'unavailable' }
    }
    await f.store.refresh()
    expect(f.store.unavailable.value).toBe(true)
    f.port.call = original
    await f.store.refresh()
    expect(f.store.unavailable.value).toBe(false)
    expect(f.store.error.value).toBeNull()
    expect(f.store.pending.value).toEqual([])
  })

  it('does not let a pre-clear inbox read restore deleted shares', async () => {
    const f = inboxFixture(['ready'])
    await f.store.refresh()
    const snapshot = structuredClone(f.records)
    const original = f.port.call
    let finish!: (records: IncomingShare[]) => void
    let first = true
    f.port.call = async (action, args) => {
      if (action === 'list' && first) {
        first = false
        return (await new Promise<IncomingShare[]>((resolve) => {
          finish = resolve
        })) as never
      }
      return original(action, args)
    }
    const reading = f.store.refresh()
    await vi.waitFor(() => expect(finish).toBeDefined())
    await f.store.discardAll()
    finish(snapshot)
    await reading
    expect(f.store.pending.value).toEqual([])
    expect(f.records).toEqual([])
  })

  it('uses the device without account credentials and never falls back automatically', async () => {
    const f = fixture()
    f.seed({ accountId: undefined, roomId: undefined })
    await f.store.open('share')
    await f.store.generate('https://example.org')
    expect(
      f.calls.filter((call) => call.action === 'device_preview'),
    ).toHaveLength(1)
    expect(f.store.selected.value?.previews?.[0]?.title).toBe('Device preview')
    expect(f.post).not.toHaveBeenCalled()
    const original = f.port.call
    f.port.call = async (action, args) => {
      if (action === 'device_preview') throw { code: 'devicePreview_timeout' }
      return original(action, args)
    }
    await f.store.generate('https://example.org')
    expect(f.store.error.value).toContain('on-device preview lookup timed out')
    expect(f.post).not.toHaveBeenCalled()
    expect(f.store.selected.value?.text).toBe('https://example.org')
  })
  it('explains a missing public IPv4 destination without changing the draft or using the homeserver', async () => {
    const f = fixture()
    await f.store.open('share')
    const input = structuredClone(f.store.selected.value)
    const original = f.port.call
    f.port.call = async (action, args) => {
      if (action === 'device_preview')
        throw { code: 'devicePreview_publicIPv4Required' }
      return original(action, args)
    }
    await f.store.generate('https://example.org')
    expect(f.store.error.value).toContain('IPv6-only sites are not supported')
    expect(f.store.error.value).toContain('link remains sendable')
    expect(f.store.selected.value).toEqual(input)
    expect(f.post).not.toHaveBeenCalled()
  })
  it('submits a URL to the homeserver only through its separate action', async () => {
    const f = fixture()
    await f.store.open('share')
    f.post.mockResolvedValueOnce({
      data: { data: { status: 'disabled' } },
      response: new Response(),
    })
    await f.store.generateUsingHomeserver('https://example.org')
    expect(f.post).toHaveBeenCalledWith(
      '/v1/accounts/{account_id}/link-previews/generate',
      expect.anything(),
    )
    expect(f.calls.some((call) => call.action === 'device_preview')).toBe(false)
    expect(f.store.selected.value?.text).toBe('https://example.org')
  })
  it('discard preserves preview uploads referenced by a sent first attachment', async () => {
    const f = fixture()
    f.seed({
      files: [
        {
          id: 'first',
          name: 'first.txt',
          mime: 'text/plain',
          bytes: 1,
          digest: 'digest',
          thumbnail: false,
          operation: {
            id: 'first-op',
            state: 'sent',
            uploadId: 'delivered-file',
            eventId: '$first',
          },
        },
        {
          id: 'second',
          name: 'second.txt',
          mime: 'text/plain',
          bytes: 1,
          digest: 'digest',
          thumbnail: false,
          operation: {
            id: 'second-op',
            state: 'failed',
            uploadId: 'unsent-file',
          },
        },
      ],
      previews: [
        {
          matched_url: 'https://example.org',
          provenance: 'generated',
          image_upload_id: 'delivered-preview',
        },
      ],
    })
    await f.store.open('share')
    await f.store.discard('share')
    expect(
      f.calls.find(({ action }) => action === 'cleanup')?.args?.uploadIds,
    ).toEqual(['unsent-file'])
  })
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

describe('photo recovery', () => {
  const photo = {
    id: 'photo',
    name: 'photo.heic',
    mime: 'image/heic',
    bytes: 90 * 1024 * 1024,
    digest: 'fixture',
    thumbnail: false,
    operation: { id: 'photo-op', state: 'ready' as const },
  }
  it('requests a bounded native thumbnail instead of reading the full original', async () => {
    const f = fixture()
    f.seed({ files: [photo] })
    const call = f.port.call.bind(f.port)
    f.port.call = async <T>(
      action: string,
      args?: Record<string, unknown>,
    ): Promise<T> =>
      action === 'photo_thumbnail'
        ? ({ bytes: btoa('bounded JPEG fixture') } as T)
        : call<T>(action, args)
    await f.store.open('share')
    const image = await f.store.thumbnail('photo')
    expect(image.type).toBe('image/jpeg')
    expect(image.size).toBe(20)
    expect(f.calls.filter(({ action }) => action === 'read')).toHaveLength(0)
    f.store.dispose()
  })
  it('serializes a mounted batch of photo previews', async () => {
    const f = fixture()
    f.seed({ files: [photo, { ...photo, id: 'second' }] })
    const call = f.port.call.bind(f.port)
    let active = 0
    let maximum = 0
    f.port.call = async <T>(
      action: string,
      args?: Record<string, unknown>,
    ): Promise<T> => {
      if (action !== 'photo_thumbnail') return call<T>(action, args)
      active += 1
      maximum = Math.max(maximum, active)
      await Promise.resolve()
      active -= 1
      return { bytes: btoa('bounded JPEG fixture') } as T
    }
    await f.store.open('share')
    await Promise.all([f.store.thumbnail('photo'), f.store.thumbnail('second')])
    expect(maximum).toBe(1)
    f.store.dispose()
  })
  it('flushes the caption before atomically removing a photo', async () => {
    const f = fixture()
    f.seed({ files: [photo] })
    await f.store.open('share')
    await f.store.edit('Keep this caption')
    await f.store.removeFile('photo')
    expect(f.record().text).toBe('Keep this caption')
    expect(f.record().files).toEqual([])
    expect(
      f.calls.filter(({ action }) => action === 'remove_file'),
    ).toHaveLength(1)
    f.store.dispose()
  })
})
