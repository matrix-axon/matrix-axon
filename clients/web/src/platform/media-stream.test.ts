import { beforeEach, describe, expect, it, vi } from 'vitest'

const calls: { cmd: string; args: unknown }[] = []
let refuse = false
/** What `media_loopback` answers: its prefix, or a rejection. */
let loopback: () => Promise<string> = () =>
  Promise.resolve('http://127.0.0.1:41234/tok/')
/** When set, each invoke waits here until the test releases it. */
let held: (() => void)[] | null = null
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (cmd: string, args: unknown) => {
    calls.push({ cmd, args })
    if (cmd === 'plugin:axon-http|media_loopback') return loopback()
    if (refuse) return Promise.reject(new Error('scope'))
    if (held === null) return Promise.resolve()
    const queue = held
    return new Promise<void>((resolve) => queue.push(resolve))
  },
  // Tauri's macOS, iOS and Linux form; Windows and Android differ only in
  // the prefix, which Tauri owns.
  convertFileSrc: (path: string, protocol: string) =>
    `${protocol}://localhost/${encodeURIComponent(path)}`,
}))

import { StreamUnavailableError } from '../media/stream-unavailable'
import {
  loopbackMediaStreamSrc,
  mediaStreamSrcFor,
  resetMediaStreamBaseForTests,
  shellMediaStreamSrc,
} from './media-stream'

const TICKET = 'AAAAbbbbCCCCddddEEEEffffGGGGhhhhIIIIjjjjKK0'

beforeEach(() => {
  calls.length = 0
  refuse = false
  held = null
  loopback = () => Promise.resolve('http://127.0.0.1:41234/tok/')
  resetMediaStreamBaseForTests()
})

describe('shellMediaStreamSrc', () => {
  it('sends the base once and maps the ticket to the scheme', async () => {
    const url = `https://axon.example/v1/media-stream/${TICKET}`
    expect(await shellMediaStreamSrc(url)).toBe(
      `axon-media://localhost/${TICKET}`,
    )
    expect(await shellMediaStreamSrc(url)).toBe(
      `axon-media://localhost/${TICKET}`,
    )
    expect(calls).toEqual([
      {
        cmd: 'plugin:axon-http|set_media_stream_base',
        args: { base: 'https://axon.example' },
      },
    ])
  })

  it('keeps a path the server is mounted under, and resends on a change', async () => {
    await shellMediaStreamSrc(
      `http://192.168.1.4:8080/axon/v1/media-stream/${TICKET}`,
    )
    await shellMediaStreamSrc(`https://other.example/v1/media-stream/${TICKET}`)
    expect(calls.map((call) => call.args)).toEqual([
      { base: 'http://192.168.1.4:8080/axon' },
      { base: 'https://other.example' },
    ])
  })

  it('refuses anything but a ticket URL, sending nothing', async () => {
    for (const url of [
      'https://axon.example/v1/rooms',
      'https://axon.example/v1/media-stream/',
      `https://axon.example/v1/media-stream/${TICKET}/more`,
    ]) {
      await expect(shellMediaStreamSrc(url)).rejects.toThrow()
    }
    expect(calls).toEqual([])
  })

  it("keeps overlapping sends in order, so the base it remembers is the plugin's", async () => {
    held = []
    const queue = held
    const first = shellMediaStreamSrc(
      `https://one.example/v1/media-stream/${TICKET}`,
    )
    const second = shellMediaStreamSrc(
      `https://two.example/v1/media-stream/${TICKET}`,
    )
    let settled = false
    void Promise.all([first, second]).then(() => {
      settled = true
    })
    // Answer whatever is waiting, newest first, until both have settled: the
    // order in which two overlapping sends would race.
    while (!settled) {
      await new Promise((resolve) => setTimeout(resolve, 0))
      queue
        .splice(0)
        .reverse()
        .forEach((resolve) => resolve())
    }
    held = null
    // The plugin now holds `two`, so `one` must be sent again.
    await shellMediaStreamSrc(`https://one.example/v1/media-stream/${TICKET}`)
    expect(calls.map((call) => call.args)).toEqual([
      { base: 'https://one.example' },
      { base: 'https://two.example' },
      { base: 'https://one.example' },
    ])
  })

  it('rejects, and sends the base again next time, when the plugin refuses it', async () => {
    refuse = true
    const url = `https://axon.example/v1/media-stream/${TICKET}`
    await expect(shellMediaStreamSrc(url)).rejects.toThrow()
    refuse = false
    await shellMediaStreamSrc(url)
    expect(calls).toHaveLength(2)
  })
})

describe('loopbackMediaStreamSrc', () => {
  it('sends the base, asks for the prefix once, and appends the ticket', async () => {
    const url = `https://axon.example/v1/media-stream/${TICKET}`
    expect(await loopbackMediaStreamSrc(url)).toBe(
      `http://127.0.0.1:41234/tok/${TICKET}`,
    )
    expect(await loopbackMediaStreamSrc(url)).toBe(
      `http://127.0.0.1:41234/tok/${TICKET}`,
    )
    expect(calls).toEqual([
      {
        cmd: 'plugin:axon-http|set_media_stream_base',
        args: { base: 'https://axon.example' },
      },
      { cmd: 'plugin:axon-http|media_loopback', args: undefined },
    ])
  })

  it('reports a server that cannot start as unavailable, and asks again next time', async () => {
    const url = `https://axon.example/v1/media-stream/${TICKET}`
    loopback = () => Promise.reject(new Error('other'))
    await expect(loopbackMediaStreamSrc(url)).rejects.toBeInstanceOf(
      StreamUnavailableError,
    )
    loopback = () => Promise.resolve('http://127.0.0.1:5/t/')
    expect(await loopbackMediaStreamSrc(url)).toBe(
      `http://127.0.0.1:5/t/${TICKET}`,
    )
  })

  it('refuses a URL that is not a ticket URL before asking for anything', async () => {
    await expect(
      loopbackMediaStreamSrc('https://axon.example/v1/media/x'),
    ).rejects.toThrow('not a stream ticket URL')
    expect(calls).toEqual([])
  })
})

describe('mediaStreamSrcFor', () => {
  it('streams on macOS, iOS, Windows and Android through the scheme', () => {
    for (const target of [
      'darwin',
      'ios',
      'windows',
      'android',
      'androideabi',
    ]) {
      expect(mediaStreamSrcFor(target)).toBe(shellMediaStreamSrc)
    }
  })

  it('streams on Linux through the loopback server', () => {
    expect(mediaStreamSrcFor('linux')).toBe(loopbackMediaStreamSrc)
  })
})
