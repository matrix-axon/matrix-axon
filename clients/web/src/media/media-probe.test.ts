import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import {
  durationMs,
  fitWithin,
  isBlankFrame,
  POSTER_JPEG_QUALITY,
  posterFrameTime,
  probeAudio,
  probeVideo,
} from './media-probe'

/**
 * The send-time probe (ADR 0142 § 7). jsdom decodes neither video nor canvas,
 * so the elements are stubs that replay what a browser fires.
 */

interface Script {
  /** What loading fires: metadata, or the element's `error`. */
  load?: 'metadata' | 'error' | 'never'
  videoWidth?: number
  videoHeight?: number
  duration?: number
  /** `readyState` after the seek; under 2 waits for `loadeddata`. */
  readyAfterSeek?: number
  /** Fire `seeked` when `currentTime` is set. */
  seeks?: boolean
  /** Whether the element has `requestVideoFrameCallback` (Safari 15.4+). */
  frameCallbacks?: boolean
  /** Seekable ranges; with none, a seek fires no `seeked` (HTML seek step 8). */
  seekable?: number
  /** Fire `loadeddata` a moment after `loadedmetadata`. */
  loadedData?: boolean
}

/** A media element that plays back `script` asynchronously, as browsers do. */
class FakeMedia extends EventTarget {
  muted = false
  preload = ''
  playsInline = false
  readyState = 0
  videoWidth: number
  videoHeight: number
  duration: number
  seekedTo: number | null = null
  srcRemoved = false
  loads = 0
  plays = 0
  paused = true
  seekable: { length: number }
  requestVideoFrameCallback?: (callback: () => void) => number
  #src = ''

  private readonly script: Script

  constructor(script: Script) {
    super()
    this.script = script
    this.videoWidth = script.videoWidth ?? 1920
    this.videoHeight = script.videoHeight ?? 1080
    this.duration = script.duration ?? 31.2501
    this.seekable = { length: script.seekable ?? 1 }
    if (script.frameCallbacks !== false)
      this.requestVideoFrameCallback = (callback) => {
        queueMicrotask(callback)
        return 1
      }
  }

  play(): Promise<void> {
    this.plays += 1
    this.paused = false
    return Promise.resolve()
  }

  pause() {
    this.paused = true
  }

  get src(): string {
    return this.#src
  }
  set src(url: string) {
    this.#src = url
    const load = this.script.load ?? 'metadata'
    if (load === 'never') return
    queueMicrotask(() => {
      if (load === 'error') {
        this.dispatchEvent(new Event('error'))
        return
      }
      this.readyState = 1
      this.dispatchEvent(new Event('loadedmetadata'))
      if (this.script.loadedData)
        setTimeout(() => {
          this.readyState = 2
          this.dispatchEvent(new Event('loadeddata'))
        }, 0)
    })
  }

  set currentTime(time: number) {
    this.seekedTo = time
    if (this.script.seeks === false || this.seekable.length === 0) return
    queueMicrotask(() => {
      this.readyState = this.script.readyAfterSeek ?? 4
      this.dispatchEvent(new Event('seeked'))
    })
  }

  removeAttribute(name: string) {
    if (name === 'src') this.srcRemoved = true
  }

  load() {
    this.loads += 1
  }

  /** Listeners still attached, by type. */
  listeners = new Map<string, number>()
  override addEventListener(
    type: string,
    listener: EventListenerOrEventListenerObject | null,
    options?: AddEventListenerOptions | boolean,
  ) {
    this.listeners.set(type, (this.listeners.get(type) ?? 0) + 1)
    super.addEventListener(type, listener, options)
  }
  override removeEventListener(
    type: string,
    listener: EventListenerOrEventListenerObject | null,
    options?: EventListenerOptions | boolean,
  ) {
    this.listeners.set(type, (this.listeners.get(type) ?? 0) - 1)
    super.removeEventListener(type, listener, options)
  }
  attached(): number {
    return [...this.listeners.values()].reduce((a, b) => a + b, 0)
  }
}

/** RGBA pixels: one flat colour, or a picture with real contrast. */
function pixels(frame: 'picture' | 'blank', count = 64): Uint8ClampedArray {
  const data = new Uint8ClampedArray(count * 4)
  for (let i = 0; i < count; i += 1) {
    const value = frame === 'blank' ? 0 : (i * 37) % 256
    data.set([value, value, value, 255], i * 4)
  }
  return data
}

class FakeCanvas {
  width = 0
  height = 0
  drawn: unknown[][] = []
  /** What each successive draw reads back; the last one repeats. */
  frames: ('picture' | 'blank')[] = ['picture']
  encoded: { type?: string; quality?: unknown } | null = null
  private readonly result: Blob | null
  private readonly context: boolean
  constructor(
    result: Blob | null = new Blob(['jpeg'], { type: 'image/jpeg' }),
    context = true,
  ) {
    this.result = result
    this.context = context
  }
  getContext() {
    return this.context
      ? {
          drawImage: (...args: unknown[]) => this.drawn.push(args),
          getImageData: () => ({
            data: pixels(
              this.frames[Math.min(this.drawn.length, this.frames.length) - 1]!,
            ),
          }),
        }
      : null
  }
  toBlob(
    callback: (blob: Blob | null) => void,
    type?: string,
    quality?: unknown,
  ) {
    this.encoded = { type, quality }
    queueMicrotask(() => callback(this.result))
  }
}

const file = () => new Blob(['moov'], { type: 'video/mp4' })

let created: string[]
let revoked: string[]
beforeEach(() => {
  created = []
  revoked = []
  let n = 0
  vi.spyOn(URL, 'createObjectURL').mockImplementation(() => {
    const url = `blob:probe-${++n}`
    created.push(url)
    return url
  })
  vi.spyOn(URL, 'revokeObjectURL').mockImplementation((url) => {
    revoked.push(url)
  })
})
afterEach(() => vi.restoreAllMocks())

function video(script: Script = {}) {
  const element = new FakeMedia(script)
  return {
    element,
    createVideo: () => element as unknown as HTMLVideoElement,
  }
}

describe('posterFrameTime (ADR 0142 § 2)', () => {
  it('takes the frame at 1 s, or half way through a clip under 2 s', () => {
    expect(posterFrameTime(31.25)).toBe(1)
    expect(posterFrameTime(2)).toBe(1)
    expect(posterFrameTime(1.5)).toBe(0.75)
    expect(posterFrameTime(0.4)).toBe(0.2)
  })

  it('falls back to frame 0 when the duration is unknown', () => {
    expect(posterFrameTime(Number.NaN)).toBe(0)
    expect(posterFrameTime(Infinity)).toBe(0)
    expect(posterFrameTime(0)).toBe(0)
  })
})

describe('fitWithin', () => {
  it('fits inside 800 × 600, keeping the aspect ratio', () => {
    expect(fitWithin(1920, 1080)).toEqual({ width: 800, height: 450 })
    expect(fitWithin(1080, 1920)).toEqual({ width: 338, height: 600 })
    expect(fitWithin(4000, 3000)).toEqual({ width: 800, height: 600 })
  })

  it('never upscales', () => {
    expect(fitWithin(640, 360)).toEqual({ width: 640, height: 360 })
  })
})

describe('durationMs', () => {
  it('rounds up, as Element does', () => {
    expect(durationMs(31.2501)).toBe(31251)
    expect(durationMs(0.0001)).toBe(1)
  })

  it('reports nothing for a duration the browser does not know', () => {
    expect(durationMs(Number.NaN)).toBeUndefined()
    expect(durationMs(Infinity)).toBeUndefined()
    expect(durationMs(0)).toBeUndefined()
  })
})

describe('probeVideo', () => {
  it('reports dimensions and duration, and draws the 1 s frame as a JPEG', async () => {
    const { element, createVideo } = video()
    const canvas = new FakeCanvas()

    const probe = await probeVideo(file(), {
      createVideo,
      createCanvas: () => canvas as unknown as HTMLCanvasElement,
    })

    expect(probe?.info).toEqual({
      width: 1920,
      height: 1080,
      duration_ms: 31251,
    })
    expect(probe?.poster?.type).toBe('image/jpeg')
    expect(element.seekedTo).toBe(1)
    expect([canvas.width, canvas.height]).toEqual([800, 450])
    expect(canvas.drawn).toEqual([[element, 0, 0, 800, 450]])
    expect(canvas.encoded).toEqual({
      type: 'image/jpeg',
      quality: POSTER_JPEG_QUALITY,
    })
    // Off-DOM playback iOS will load, emptied and released afterwards.
    expect(element.muted).toBe(true)
    expect(element.playsInline).toBe(true)
    expect(element.srcRemoved).toBe(true)
    expect(element.loads).toBe(1)
    expect(revoked).toEqual(created)
    expect(element.attached()).toBe(0)
  })

  it('omits a duration the browser does not know, and keeps the rest', async () => {
    const { element, createVideo } = video({ duration: Infinity })

    const probe = await probeVideo(file(), {
      createVideo,
      createCanvas: () => new FakeCanvas() as unknown as HTMLCanvasElement,
    })

    expect(probe?.info).toEqual({ width: 1920, height: 1080 })
    expect(element.seekedTo).toBe(0)
  })

  it('waits for a frame when the seek lands before one is decoded', async () => {
    const { element, createVideo } = video({ readyAfterSeek: 1 })
    const canvas = new FakeCanvas()

    const pending = probeVideo(file(), {
      createVideo,
      createCanvas: () => canvas as unknown as HTMLCanvasElement,
    })
    await vi.waitFor(() => expect(element.seekedTo).toBe(1))
    await Promise.resolve()
    expect(canvas.drawn).toEqual([])
    element.readyState = 2
    element.dispatchEvent(new Event('loadeddata'))

    expect((await pending)?.poster).not.toBeNull()
    expect(canvas.drawn).toHaveLength(1)
  })

  it('reports nothing for a codec the browser cannot decode', async () => {
    const { element, createVideo } = video({ load: 'error' })

    expect(await probeVideo(file(), { createVideo })).toBeNull()
    expect(revoked).toEqual(created)
    expect(element.srcRemoved).toBe(true)
    expect(element.attached()).toBe(0)
  })

  it('reports nothing for a file with no picture', async () => {
    // An audio-only file, or a video track that cannot be decoded.
    const { createVideo } = video({ videoWidth: 0, videoHeight: 0 })

    expect(await probeVideo(file(), { createVideo })).toBeNull()
  })

  it('gives up when the budget runs out, and leaves nothing attached', async () => {
    const { element, createVideo } = video({ seeks: false })

    const probe = await probeVideo(file(), { createVideo, timeoutMs: 20 })

    expect(probe).toBeNull()
    expect(revoked).toEqual(created)
    expect(element.srcRemoved).toBe(true)
    expect(element.attached()).toBe(0)
  })

  it('keeps the info, with no poster, when the frame cannot be encoded', async () => {
    const info = { width: 1920, height: 1080, duration_ms: 31251 }
    expect(
      await probeVideo(file(), {
        createVideo: video().createVideo,
        createCanvas: () =>
          new FakeCanvas(null) as unknown as HTMLCanvasElement,
      }),
    ).toEqual({ info, poster: null })
    expect(
      await probeVideo(file(), {
        createVideo: video().createVideo,
        createCanvas: () =>
          new FakeCanvas(undefined, false) as unknown as HTMLCanvasElement,
      }),
    ).toEqual({ info, poster: null })
  })

  it('plays for a moment and draws again when the frame comes out blank', async () => {
    // iPhone Safari: seeked, a frame reported, and solid black drawn.
    const { element, createVideo } = video()
    const canvas = new FakeCanvas()
    canvas.frames = ['blank', 'picture']

    const probe = await probeVideo(file(), {
      createVideo,
      createCanvas: () => canvas as unknown as HTMLCanvasElement,
    })

    expect(probe?.poster).not.toBeNull()
    expect(canvas.drawn).toHaveLength(2)
    expect(element.plays).toBe(1)
    expect(element.paused).toBe(true)
  })

  it('sends no poster, but keeps the info, when every frame is blank', async () => {
    const { element, createVideo } = video()
    const canvas = new FakeCanvas()
    canvas.frames = ['blank']

    const probe = await probeVideo(file(), {
      createVideo,
      createCanvas: () => canvas as unknown as HTMLCanvasElement,
    })

    expect(probe).toEqual({
      info: { width: 1920, height: 1080, duration_ms: 31251 },
      poster: null,
    })
    expect(canvas.drawn).toHaveLength(3)
    expect(canvas.encoded).toBeNull()
    expect(element.paused).toBe(true)
  })

  it('draws the loaded frame, without waiting for a seek, when nothing is seekable', async () => {
    // A recording whose duration is not known: the seek would end silently.
    const { element, createVideo } = video({
      duration: Infinity,
      seekable: 0,
      loadedData: true,
    })

    const probe = await probeVideo(file(), {
      createVideo,
      createCanvas: () => new FakeCanvas() as unknown as HTMLCanvasElement,
      timeoutMs: 1_000,
    })

    expect(probe?.info).toEqual({ width: 1920, height: 1080 })
    expect(probe?.poster).not.toBeNull()
    expect(element.seekedTo).toBeNull()
  })

  it('draws anyway when the browser never says a frame was presented', async () => {
    const { createVideo } = video({ frameCallbacks: false })

    const probe = await probeVideo(file(), {
      createVideo,
      createCanvas: () => new FakeCanvas() as unknown as HTMLCanvasElement,
      frameWaitMs: 5,
    })

    expect(probe?.poster).not.toBeNull()
  })

  it('drops a poster over the 2 MiB the server accepts', async () => {
    const big = new Blob([new Uint8Array(2 * 1024 * 1024 + 1)], {
      type: 'image/jpeg',
    })
    expect(
      (
        await probeVideo(file(), {
          createVideo: video().createVideo,
          createCanvas: () =>
            new FakeCanvas(big) as unknown as HTMLCanvasElement,
        })
      )?.poster,
    ).toBeNull()
  })
})

describe('isBlankFrame', () => {
  it('calls one flat colour blank, transparent included', () => {
    expect(isBlankFrame(pixels('blank'))).toBe(true)
    const grey = new Uint8ClampedArray(64 * 4).fill(128)
    expect(isBlankFrame(grey)).toBe(true)
    // An undrawn canvas: transparent pixels, which JPEG paints black.
    expect(isBlankFrame(new Uint8ClampedArray(64 * 4))).toBe(true)
    expect(isBlankFrame(new Uint8ClampedArray(0))).toBe(true)
  })

  it('calls a frame with contrast a picture, however dark', () => {
    expect(isBlankFrame(pixels('picture'))).toBe(false)
    const dim = pixels('blank')
    dim.set([40, 40, 40, 255], 32 * 4)
    expect(isBlankFrame(dim)).toBe(false)
  })
})

describe('probeAudio', () => {
  const audio = (script: Script) => {
    const element = new FakeMedia(script)
    return {
      element,
      createAudio: () => element as unknown as HTMLAudioElement,
    }
  }

  it('reports the duration, rounded up', async () => {
    const { element, createAudio } = audio({ duration: 4.1999 })

    expect(await probeAudio(file(), { createAudio })).toEqual({
      info: { duration_ms: 4200 },
      poster: null,
    })
    expect(element.srcRemoved).toBe(true)
    expect(revoked).toEqual(created)
  })

  it('reports nothing when the duration is unknown or the file will not load', async () => {
    expect(await probeAudio(file(), audio({ duration: Infinity }))).toBeNull()
    expect(await probeAudio(file(), audio({ load: 'error' }))).toBeNull()
    expect(
      await probeAudio(file(), { ...audio({ load: 'never' }), timeoutMs: 20 }),
    ).toBeNull()
  })
})
