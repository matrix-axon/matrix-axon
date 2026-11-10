import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

/** A stand-in for `src-tauri/axon-link-preview`: each lookup waits for the test. */
interface Call {
  cmd: string
  args: { id: number; url?: string }
}
const calls: Call[] = []
let answer: (call: Call) => Promise<unknown> = () => new Promise(() => {})
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (cmd: string, args: Call['args']) => {
    const call = { cmd, args }
    calls.push(call)
    return answer(call)
  },
}))

import { LinkPreviewError } from './index'
import { decodeFrame, shellLinkPreview } from './link-preview'

/** A frame as `frame` in `axon-link-preview/src/lib.rs` builds it. */
function frame(head: unknown, image: number[] = []): ArrayBuffer {
  const json = new TextEncoder().encode(JSON.stringify(head))
  const bytes = new Uint8Array(4 + json.length + image.length)
  new DataView(bytes.buffer).setUint32(0, json.length)
  bytes.set(json, 4)
  bytes.set(image, 4 + json.length)
  return bytes.buffer
}

const HEAD = {
  url: 'https://example.org/a',
  title: 'Title',
  description: 'About',
  siteName: 'Site',
  image: { mimeType: 'image/jpeg', width: 1200, height: 600, byteLength: 3 },
  imageUnavailable: false,
}

beforeEach(() => {
  calls.length = 0
  answer = () => new Promise(() => {})
})
afterEach(() => {
  vi.restoreAllMocks()
})

describe('decodeFrame', () => {
  it('reads the head and the image bytes after it', () => {
    const preview = decodeFrame(frame(HEAD, [1, 2, 3]))
    expect(preview).toEqual({
      url: 'https://example.org/a',
      title: 'Title',
      description: 'About',
      siteName: 'Site',
      image: {
        bytes: new Uint8Array([1, 2, 3]),
        mimeType: 'image/jpeg',
        width: 1200,
        height: 600,
      },
      imageUnavailable: false,
    })
  })

  it('reads a text-only preview', () => {
    expect(
      decodeFrame(
        frame({ url: 'https://example.org/', imageUnavailable: true }),
      ),
    ).toEqual({ url: 'https://example.org/', imageUnavailable: true })
  })

  it.each([
    ['a short frame', new Uint8Array([0, 0]).buffer],
    ['a length past the end', frame(HEAD, [1, 2, 3]).slice(0, 10)],
    [
      'bytes the head does not account for',
      frame({ url: 'u', imageUnavailable: false }, [1]),
    ],
    ['an image length that differs', frame(HEAD, [1, 2])],
    [
      'an unsupported image type',
      frame(
        { ...HEAD, image: { ...HEAD.image, mimeType: 'image/svg+xml' } },
        [1, 2, 3],
      ),
    ],
    [
      'an image larger than the plugin makes',
      frame({ ...HEAD, image: { ...HEAD.image, width: 1201 } }, [1, 2, 3]),
    ],
    [
      'a title over the send limit',
      frame({ ...HEAD, title: 'a'.repeat(301) }, [1, 2, 3]),
    ],
    ['an empty site name', frame({ ...HEAD, siteName: '' }, [1, 2, 3])],
    ['a missing url', frame({ imageUnavailable: false })],
  ])('refuses %s', (_name, buffer) => {
    expect(() => decodeFrame(buffer)).toThrow(LinkPreviewError)
  })
})

describe('shellLinkPreview', () => {
  it('asks the plugin with a fresh id and resolves with the decoded frame', async () => {
    answer = () => Promise.resolve(frame(HEAD, [1, 2, 3]))
    const first = await shellLinkPreview('https://example.org/a')
    await shellLinkPreview('https://example.org/b')
    expect(first.title).toBe('Title')
    expect(calls.map((call) => call.cmd)).toEqual([
      'plugin:axon-link-preview|lookup',
      'plugin:axon-link-preview|lookup',
    ])
    expect(calls[0]!.args.url).toBe('https://example.org/a')
    expect(calls[1]!.args.id).toBe(calls[0]!.args.id + 1)
  })

  it.each([
    'unsupported_url',
    'public_ipv4_required',
    'timeout',
    'no_metadata',
    'unavailable',
    'busy',
  ])('rejects with the plugin kind %s', async (kind) => {
    answer = () => Promise.reject({ kind })
    await expect(shellLinkPreview('https://example.org/')).rejects.toEqual(
      new LinkPreviewError(kind as never),
    )
  })

  it('reports anything else as unavailable', async () => {
    for (const error of [
      { kind: 'cancelled' },
      'a string',
      null,
      { kind: 7 },
    ]) {
      answer = () => Promise.reject(error)
      const rejection = await shellLinkPreview('https://example.org/').catch(
        (e: unknown) => e,
      )
      expect(rejection).toBeInstanceOf(LinkPreviewError)
      expect((rejection as LinkPreviewError).kind).toBe('unavailable')
    }
  })

  it('never asks when the signal has already aborted', async () => {
    const controller = new AbortController()
    controller.abort()
    await expect(
      shellLinkPreview('https://example.org/', controller.signal),
    ).rejects.toBe(controller.signal.reason)
    expect(calls).toEqual([])
  })

  it('rejects with the abort reason at once and cancels that id', async () => {
    let settle: (frame: ArrayBuffer) => void = () => {}
    answer = (call) =>
      call.cmd.endsWith('|lookup')
        ? new Promise((resolve) => (settle = resolve))
        : Promise.resolve(null)
    const controller = new AbortController()
    const pending = shellLinkPreview('https://example.org/', controller.signal)
    const reason = new DOMException('gone', 'AbortError')
    controller.abort(reason)
    await expect(pending).rejects.toBe(reason)
    const lookup = calls[0]!
    expect(calls[1]).toEqual({
      cmd: 'plugin:axon-link-preview|cancel',
      args: { id: lookup.args.id },
    })
    // A late answer changes nothing.
    settle(frame(HEAD, [1, 2, 3]))
    await Promise.resolve()
  })

  it('does not cancel a lookup that already finished', async () => {
    answer = () => Promise.resolve(frame(HEAD, [1, 2, 3]))
    const controller = new AbortController()
    await shellLinkPreview('https://example.org/', controller.signal)
    controller.abort()
    expect(calls.map((call) => call.cmd)).toEqual([
      'plugin:axon-link-preview|lookup',
    ])
  })
})
