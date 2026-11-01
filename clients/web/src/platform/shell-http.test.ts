import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

/**
 * A stand-in for `src-tauri/axon-http`: each `send` waits until the test
 * answers it, so a test can put a cancel or an abort anywhere in a request's
 * life.
 */
interface Call {
  cmd: string
  args: unknown
  options?: { headers?: Record<string, string> }
}
/**
 * A stand-in for the engine's `FinalizationRegistry`, so a test can see what
 * the binding registers and run its cleanup as a collection would.
 */
const finalizer = vi.hoisted(() => {
  const state = {
    cleanup: null as ((held: unknown) => void) | null,
    live: new Map<object, { target: object; held: unknown }>(),
  }
  globalThis.FinalizationRegistry = class<T> {
    constructor(cleanup: (held: T) => void) {
      state.cleanup = cleanup as (held: unknown) => void
    }
    register(target: object, held: T, token: object) {
      state.live.set(token, { target, held })
    }
    unregister(token: object) {
      return state.live.delete(token)
    }
  } as unknown as FinalizationRegistryConstructor
  return state
})

const calls: Call[] = []
let onInvoke: (call: Call) => Promise<unknown> = () => Promise.resolve(null)
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (cmd: string, args?: unknown, options?: Call['options']) => {
    const call = { cmd, args, options }
    calls.push(call)
    return onInvoke(call)
  },
}))

import {
  resetShellTransport,
  shellFetch,
  shellTransportStats,
} from './shell-http'
import { transportFailureOf, transportVersion } from './transport-report'
import { File as NodeFile } from 'node:buffer'

const URL_ = 'https://axon.example/v1/rooms?limit=5'

interface Line {
  id: number
  method: string
  url: string
  headers: [string, string][]
}

function lineOf(call: Call): Line {
  const header = call.options?.headers?.['axon-http-request']
  return JSON.parse(decodeURIComponent(header!)) as Line
}

function sends(): Call[] {
  return calls.filter((call) => call.cmd === 'plugin:axon-http|send')
}

/** A body frame: the chunk and a trailing `0`. */
function chunk(text: string): ArrayBuffer {
  const bytes = new TextEncoder().encode(text)
  const frame = new Uint8Array(bytes.length + 1)
  frame.set(bytes)
  return frame.buffer
}
const END = new Uint8Array([1]).buffer

/** The plugin answering `send` with `head`, and `read_body` with `frames`. */
function plugin(
  head: Record<string, unknown>,
  frames: ArrayBuffer[] = [],
): void {
  const queue = [...frames]
  onInvoke = (call) => {
    switch (call.cmd) {
      case 'plugin:axon-http|send':
        return Promise.resolve({
          status: 200,
          headers: [],
          url: URL_,
          version: 'h2',
          body: true,
          ...head,
        })
      case 'plugin:axon-http|read_body':
        return Promise.resolve(queue.shift() ?? END)
      default:
        return Promise.resolve(null)
    }
  }
}

function never(): AbortSignal {
  return new AbortController().signal
}

beforeEach(() => {
  calls.length = 0
  onInvoke = () => Promise.resolve(null)
})

afterEach(() => {
  vi.restoreAllMocks()
})

describe('shellFetch request', () => {
  it('sends the line in one invoke header and the body raw', async () => {
    plugin({ body: false, status: 204 })
    await shellFetch(
      URL_,
      {
        method: 'put',
        headers: { Authorization: 'Bearer t', 'Content-Type': 'text/plain' },
        body: 'hello',
      },
      never(),
    )

    const [send] = sends()
    const line = lineOf(send!)
    expect(line.method).toBe('PUT')
    expect(line.url).toBe(URL_)
    expect(line.headers).toEqual(
      expect.arrayContaining([
        ['authorization', 'Bearer t'],
        ['content-type', 'text/plain'],
      ]),
    )
    // The raw payload, not a JSON array of numbers (ADR 0127 § 3).
    expect(send!.args).toBeInstanceOf(Uint8Array)
    expect(new TextDecoder().decode(send!.args as Uint8Array)).toBe('hello')
  })

  it('sends an upload as the bytes of the file', async () => {
    plugin({ body: false, status: 204 })
    // Node's `File`, not jsdom's: Node's `Request` reads only its own blobs,
    // and stringifies jsdom's. A webview has one kind of each.
    const file = new NodeFile([new Uint8Array([0, 1, 2, 255])], 'a.bin')
    await shellFetch(URL_, { method: 'POST', body: file as Blob }, never())

    expect(Array.from(sends()[0]!.args as Uint8Array)).toEqual([0, 1, 2, 255])
  })

  it('takes a Request as openapi-fetch builds one', async () => {
    plugin({ body: false, status: 204 })
    await shellFetch(
      new Request(URL_, { method: 'DELETE', headers: { 'x-a': '1' } }),
      undefined,
      never(),
    )
    const line = lineOf(sends()[0]!)
    expect(line.method).toBe('DELETE')
    expect(line.headers).toEqual([['x-a', '1']])
    expect((sends()[0]!.args as Uint8Array).length).toBe(0)
  })

  it('gives every request its own id', async () => {
    plugin({ body: false, status: 204 })
    await shellFetch(URL_, undefined, never())
    await shellFetch(URL_, undefined, never())
    const [first, second] = sends().map((call) => lineOf(call).id)
    expect(second).toBeGreaterThan(first!)
  })

  it('refuses a redirect mode the transport cannot honour', async () => {
    // The plugin always follows redirects. Ignoring `manual` would hand the
    // caller a followed response it asked not to get.
    for (const redirect of ['manual', 'error'] as const) {
      await expect(
        shellFetch(URL_, { redirect }, never()),
      ).rejects.toBeInstanceOf(TypeError)
    }
    expect(calls).toEqual([])
  })

  it('does nothing for a signal that has already fired', async () => {
    const controller = new AbortController()
    controller.abort(new DOMException('gone', 'AbortError'))
    await expect(
      shellFetch(URL_, undefined, controller.signal),
    ).rejects.toMatchObject({ name: 'AbortError' })
    expect(calls).toEqual([])
  })
})

describe('shellFetch response', () => {
  it('is a real Response, typed at construction, with its url and version', async () => {
    plugin(
      {
        status: 200,
        headers: [
          ['content-type', 'image/png'],
          ['x-b', '2'],
        ],
        url: 'https://media.example/after-redirect',
        version: 'h2',
      },
      [chunk('ab'), chunk(''), chunk('c')],
    )
    const response = await shellFetch(URL_, undefined, never())

    expect(response.status).toBe(200)
    expect(response.url).toBe('https://media.example/after-redirect')
    expect(response.headers.get('x-b')).toBe('2')
    expect(transportVersion(response)).toBe('h2')
    // The workaround in `media-service.ts` exists because the legacy plugin's
    // `blob()` came back untyped. This one does not.
    const blob = await response.clone().blob()
    expect(blob.type).toBe('image/png')
    expect(await response.text()).toBe('abc')
  })

  it('keeps its url on a clone', async () => {
    plugin({ url: 'https://h/final' }, [chunk('x')])
    const response = await shellFetch(URL_, undefined, never())
    const copy = response.clone()
    expect(copy.url).toBe('https://h/final')
    expect(transportVersion(copy)).toBe('h2')
    await response.text()
    await copy.text()
  })

  it('reads no body the transport never stored', async () => {
    plugin({ status: 204, body: false })
    const response = await shellFetch(URL_, undefined, never())
    expect(response.body).toBeNull()
    expect(calls.map((call) => call.cmd)).toEqual(['plugin:axon-http|send'])
  })

  it('records only a version it knows', async () => {
    plugin({ version: 'spdy/3', body: false, status: 204 })
    const response = await shellFetch(URL_, undefined, never())
    expect(transportVersion(response)).toBeNull()
  })

  it('releases a body the reader cancels', async () => {
    plugin({}, [chunk('a'), chunk('b')])
    const response = await shellFetch(URL_, undefined, never())
    const reader = response.body!.getReader()
    await reader.read()
    await reader.cancel()
    const id = lineOf(sends()[0]!).id
    expect(
      calls.filter((call) => call.cmd === 'plugin:axon-http|drop_body'),
    ).toEqual([expect.objectContaining({ args: { id } })])
  })

  it('does not release a body that ended, nor cancel it later', async () => {
    plugin({}, [chunk('a')])
    const controller = new AbortController()
    const response = await shellFetch(URL_, undefined, controller.signal)
    await response.text()
    controller.abort()
    expect(
      calls.filter(
        (call) =>
          call.cmd === 'plugin:axon-http|drop_body' ||
          call.cmd === 'plugin:axon-http|cancel',
      ),
    ).toEqual([])
  })
})

describe('shellFetch unread bodies', () => {
  it('registers an object of its own for release, not an engine object', async () => {
    // Chromium drops and recreates the script wrapper of a `ReadableStream`
    // or `Response` nothing in script refers to, even mid-read, and that
    // wrapper's finalizer then fires. A finalizer on one released bodies that
    // were still being read.
    plugin({}, [chunk('a')])
    finalizer.live.clear()
    const response = await shellFetch(URL_, undefined, never())
    const id = lineOf(sends()[0]!).id
    const entries = [...finalizer.live.values()]
    expect(entries).toEqual([{ target: expect.any(Object), held: id }])
    const { target } = entries[0]!
    expect(target).not.toBeInstanceOf(ReadableStream)
    expect(target).not.toBeInstanceOf(Response)
    expect(target).not.toBe(response.body)

    // Read to the end, the body is the plugin's to forget, not the finalizer's.
    await response.text()
    expect(finalizer.live.size).toBe(0)
  })

  it('releases the body when the finalizer runs', async () => {
    plugin({})
    finalizer.live.clear()
    await shellFetch(URL_, undefined, never())
    const id = lineOf(sends()[0]!).id
    const [{ held }] = [...finalizer.live.values()]
    finalizer.cleanup!(held)
    expect(
      calls.filter((call) => call.cmd === 'plugin:axon-http|drop_body'),
    ).toEqual([expect.objectContaining({ args: { id } })])
  })
})

describe('shellFetch failure', () => {
  it('rejects with a TypeError carrying the kind and detail', async () => {
    onInvoke = () =>
      Promise.reject({ kind: 'connect_timeout', detail: 'TimedOut' })
    const error = await shellFetch(URL_, undefined, never()).catch(
      (caught: unknown) => caught,
    )
    expect(error).toBeInstanceOf(TypeError)
    expect(transportFailureOf(error)).toEqual({
      kind: 'connect_timeout',
      detail: 'TimedOut',
    })
    // No URL, no host: the message is the kind alone.
    expect((error as Error).message).not.toContain('axon.example')
  })

  it('drops a detail that is not a variant name', async () => {
    onInvoke = () =>
      Promise.reject({ kind: 'tls', detail: 'cert for axon.example expired' })
    const error = await shellFetch(URL_, undefined, never()).catch(
      (caught: unknown) => caught,
    )
    expect(transportFailureOf(error)).toEqual({ kind: 'tls', detail: null })
  })

  it('reads a rejection that is not the transport’s as other', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    onInvoke = () => Promise.reject('axon-http.send not allowed')
    const error = await shellFetch(URL_, undefined, never()).catch(
      (caught: unknown) => caught,
    )
    expect(error).toBeInstanceOf(TypeError)
    expect(transportFailureOf(error)).toEqual({ kind: 'other', detail: null })
    expect(warn).toHaveBeenCalled()
  })

  it('fails the body read with the kind', async () => {
    plugin({})
    const send = onInvoke
    onInvoke = (call) =>
      call.cmd === 'plugin:axon-http|read_body'
        ? Promise.reject({ kind: 'body', detail: 'UnexpectedEof' })
        : send(call)
    const response = await shellFetch(URL_, undefined, never())
    const error = await response.text().catch((caught: unknown) => caught)
    expect(error).toBeInstanceOf(TypeError)
    expect(transportFailureOf(error)).toEqual({
      kind: 'body',
      detail: 'UnexpectedEof',
    })
  })
})

describe('shellFetch abort', () => {
  it('rejects with the reason and cancels, before send returns', async () => {
    let answer: ((value: unknown) => void) | null = null
    onInvoke = (call) =>
      call.cmd === 'plugin:axon-http|send'
        ? new Promise((resolve) => (answer = resolve))
        : Promise.resolve(null)
    const controller = new AbortController()
    const pending = shellFetch(URL_, undefined, controller.signal)
    await vi.waitFor(() => expect(sends()).toHaveLength(1))

    controller.abort(new DOMException('deadline', 'TimeoutError'))
    await expect(pending).rejects.toMatchObject({ name: 'TimeoutError' })
    const id = lineOf(sends()[0]!).id
    expect(
      calls.filter((call) => call.cmd === 'plugin:axon-http|cancel'),
    ).toEqual([expect.objectContaining({ args: { id } })])
    // An answer after the abort changes nothing.
    answer!({ status: 200, headers: [], url: URL_, version: 'h2', body: false })
  })

  it('is an AbortError even when the transport’s cancelled arrives first', async () => {
    // The plugin answers the cancel by failing the send, and here it does so
    // inside the cancel call itself, ahead of anything else hearing the abort.
    let fail: ((error: unknown) => void) | null = null
    onInvoke = (call) => {
      if (call.cmd === 'plugin:axon-http|send') {
        return new Promise((_, reject) => (fail = reject))
      }
      if (call.cmd === 'plugin:axon-http|cancel') {
        fail?.({ kind: 'cancelled', detail: null })
      }
      return Promise.resolve(null)
    }
    const controller = new AbortController()
    const pending = shellFetch(URL_, undefined, controller.signal)
    await vi.waitFor(() => expect(sends()).toHaveLength(1))
    controller.abort()
    const error = await pending.catch((caught: unknown) => caught)
    expect(error).toMatchObject({ name: 'AbortError' })
    expect(transportFailureOf(error)).toBeNull()
  })

  it('is an AbortError when the cancelled answer is what the caller awaits', async () => {
    // The other order: the send fails with `cancelled` and only then is the
    // signal seen as aborted. The reason still wins.
    const controller = new AbortController()
    onInvoke = (call) => {
      if (call.cmd === 'plugin:axon-http|send') {
        controller.abort()
        return Promise.reject({ kind: 'cancelled', detail: null })
      }
      return Promise.resolve(null)
    }
    await expect(
      shellFetch(URL_, undefined, controller.signal),
    ).rejects.toMatchObject({ name: 'AbortError' })
  })

  it('fails a body mid-read with the reason, and cancels it', async () => {
    plugin({}, [chunk('a')])
    const controller = new AbortController()
    const response = await shellFetch(URL_, undefined, controller.signal)
    const reader = response.body!.getReader()
    expect((await reader.read()).done).toBe(false)
    onInvoke = (call) =>
      call.cmd === 'plugin:axon-http|read_body'
        ? new Promise(() => {})
        : Promise.resolve(null)
    const read = reader.read()
    controller.abort(new DOMException('deadline', 'TimeoutError'))
    await expect(read).rejects.toMatchObject({ name: 'TimeoutError' })
    const id = lineOf(sends()[0]!).id
    expect(
      calls.filter((call) => call.cmd === 'plugin:axon-http|cancel'),
    ).toEqual([expect.objectContaining({ args: { id } })])
  })
})

describe('shellFetch abort between headers and body', () => {
  // The abort lands a few microtasks after `send` answers: before, inside or
  // after the handoff from the headers to the body stream. Wherever it lands,
  // the caller sees its own reason, from `shellFetch` or from the first read,
  // and never the plugin's `cancelled` for the entry the cancel removed.
  const reasons = [
    () => new DOMException('gone', 'AbortError'),
    () => new DOMException('deadline', 'TimeoutError'),
    () => new Error('a custom reason'),
  ]
  for (const makeReason of reasons) {
    for (let hops = 0; hops <= 5; hops++) {
      it(`keeps the reason (${makeReason().name}, ${hops} hops)`, async () => {
        const reason = makeReason()
        const controller = new AbortController()
        onInvoke = (call) => {
          switch (call.cmd) {
            case 'plugin:axon-http|send': {
              const head = Promise.resolve({
                status: 200,
                headers: [],
                url: URL_,
                version: 'h2',
                body: true,
              })
              let later: Promise<unknown> = head
              for (let hop = 0; hop < hops; hop++) {
                later = later.then(() => undefined)
              }
              void later.then(() => controller.abort(reason))
              return head
            }
            case 'plugin:axon-http|read_body':
              // As the plugin does once a cancel has removed the entry, and
              // otherwise a read that waits on the socket.
              return controller.signal.aborted
                ? Promise.reject({ kind: 'cancelled', detail: null })
                : new Promise(() => {})
            default:
              return Promise.resolve(null)
          }
        }
        const outcome = await shellFetch(URL_, undefined, controller.signal)
          .then((response) => response.body!.getReader().read())
          .catch((error: unknown) => error)
        expect(outcome).toBe(reason)
        expect(
          calls.filter((call) => call.cmd === 'plugin:axon-http|cancel'),
        ).toHaveLength(1)
      })
    }
  }
})

describe('shell transport upkeep', () => {
  it('resets and reads the counters through the plugin', async () => {
    const stats = {
      connectionsOpened: 1,
      requestsSent: 2,
      retries: { nack: 0, fastReset: 0, requeue: 0 },
      resets: 1,
      liveEntries: 0,
    }
    onInvoke = (call) =>
      Promise.resolve(call.cmd === 'plugin:axon-http|stats' ? stats : null)
    await resetShellTransport()
    await expect(shellTransportStats()).resolves.toEqual(stats)
    expect(calls.map((call) => call.cmd)).toEqual([
      'plugin:axon-http|reset',
      'plugin:axon-http|stats',
    ])
  })
})
