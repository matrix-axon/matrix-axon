import type { components } from '../api/schema'

/**
 * Send-time metadata for video and audio (ADR 0142 § 7, issue 24).
 *
 * The server makes still-image thumbnails, but it has no video decoder, and
 * the browser does. So before a video is sent the web client loads it into an
 * off-DOM `<video>`, reads its dimensions and duration, and draws one frame to
 * a canvas as the poster. An audio file gets its duration the same way, from
 * an `<audio>`. The server validates and re-encodes the poster (§ 4), so this
 * only has to produce something close to § 2's standard.
 *
 * Everything here is best-effort. A codec the browser cannot decode (HEVC on
 * some Chromium builds), a file that never loads, or any step past the 10 s
 * budget yields `null`, and the media is sent without a poster and without
 * `info`: a poster never blocks a send.
 */

export type SendMediaInfo = components['schemas']['SendMediaInfo']

/** What a probe learned, ready for `send-media`. */
export interface MediaProbe {
  info: SendMediaInfo
  /** A JPEG poster frame, for a video. */
  poster: Blob | null
}

/** ADR 0142 § 2: thumbnails fit inside 800 × 600 and are never upscaled. */
export const POSTER_MAX_WIDTH = 800
export const POSTER_MAX_HEIGHT = 600
/** ADR 0142 § 2: JPEG at quality 78. */
export const POSTER_JPEG_QUALITY = 0.78
/**
 * The largest thumbnail `send-media` accepts (`claim_preview_upload`). An
 * 800 × 600 JPEG is far below it; a larger one would be a `400` that fails
 * the whole send, so it is dropped instead.
 */
export const MAX_POSTER_BYTES = 2 * 1024 * 1024
/** ADR 0142 § 7: give up after 10 s. */
export const PROBE_TIMEOUT_MS = 10_000

/** `HTMLMediaElement.HAVE_CURRENT_DATA`: a frame is there to draw. */
const HAVE_CURRENT_DATA = 2
/**
 * How long to wait for the browser to present a frame before drawing anyway.
 * `requestVideoFrameCallback` may never fire for a paused, off-DOM video, so
 * it is only ever a hint.
 */
const FRAME_WAIT_MS = 500
/**
 * A frame whose luma varies by no more than this (of 255) is blank. iPhone
 * Safari can report a seeked, decoded frame and still draw solid black, and a
 * black poster is worse than the placeholder a missing one leaves.
 */
const BLANK_LUMA_SPREAD = 8
/** Pixels sampled for the blank check, spread evenly over the frame. */
const BLANK_SAMPLES = 4096
/** A Matrix integer's ceiling, which `info` values must fit. */
const MAX_MATRIX_INT = Number.MAX_SAFE_INTEGER

/** Seams for tests: jsdom decodes neither video nor canvas. */
export interface ProbeDeps {
  createVideo?: () => HTMLVideoElement
  createAudio?: () => HTMLAudioElement
  createCanvas?: () => HTMLCanvasElement
  timeoutMs?: number
  /** [`FRAME_WAIT_MS`], shortened by tests. */
  frameWaitMs?: number
}

/**
 * The time to take the poster from (ADR 0142 § 2): 1 s in, or half way
 * through a video shorter than 2 s. Frame 0 is often black or a fade-in.
 * A duration the browser does not know (`NaN`, or `Infinity` for some WebM
 * recordings) gets frame 0, which is at least a frame.
 */
export function posterFrameTime(durationSeconds: number): number {
  if (!Number.isFinite(durationSeconds) || durationSeconds <= 0) return 0
  return durationSeconds < 2 ? durationSeconds / 2 : 1
}

/** The size that fits `width` × `height` inside the box, never upscaled. */
export function fitWithin(
  width: number,
  height: number,
  maxWidth = POSTER_MAX_WIDTH,
  maxHeight = POSTER_MAX_HEIGHT,
): { width: number; height: number } {
  const scale = Math.min(1, maxWidth / width, maxHeight / height)
  return {
    width: Math.max(1, Math.round(width * scale)),
    height: Math.max(1, Math.round(height * scale)),
  }
}

/**
 * A media element's duration as `duration_ms`, rounded up as Element does,
 * or `undefined` when the browser does not know it.
 */
export function durationMs(durationSeconds: number): number | undefined {
  if (!Number.isFinite(durationSeconds) || durationSeconds <= 0)
    return undefined
  const ms = Math.ceil(durationSeconds * 1000)
  return ms <= MAX_MATRIX_INT ? ms : undefined
}

/** A dimension `info` can carry: a positive Matrix integer. */
function dimension(value: number): boolean {
  return Number.isInteger(value) && value > 0 && value <= MAX_MATRIX_INT
}

/**
 * Resolve on the first of `types`, reject on the element's `error` or when
 * `signal` aborts. Listeners are removed either way, so an abandoned probe
 * leaves nothing behind on the element.
 */
function once(
  target: EventTarget,
  types: readonly string[],
  signal: AbortSignal,
): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal.aborted) {
      reject(new Error('aborted'))
      return
    }
    const settle = (outcome: () => void) => {
      for (const type of types) target.removeEventListener(type, onEvent)
      target.removeEventListener('error', onError)
      signal.removeEventListener('abort', onAbort)
      outcome()
    }
    const onEvent = () => settle(resolve)
    const onError = () => settle(() => reject(new Error('media error')))
    const onAbort = () => settle(() => reject(new Error('aborted')))
    for (const type of types) target.addEventListener(type, onEvent)
    target.addEventListener('error', onError)
    signal.addEventListener('abort', onAbort)
  })
}

/** `promise`, or a rejection once `signal` aborts. */
function abortable<T>(promise: Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    if (signal.aborted) {
      reject(new Error('aborted'))
      return
    }
    const onAbort = () => reject(new Error('aborted'))
    signal.addEventListener('abort', onAbort, { once: true })
    promise.then(
      (value) => {
        signal.removeEventListener('abort', onAbort)
        resolve(value)
      },
      (err: unknown) => {
        signal.removeEventListener('abort', onAbort)
        reject(err instanceof Error ? err : new Error(String(err)))
      },
    )
  })
}

/**
 * Load `file` into `element` and run `read` once its metadata is in, within
 * one budget. The element is emptied and the object URL revoked on every
 * path, so a probe never keeps a decoder or the file's bytes alive.
 */
async function withLoaded<E extends HTMLMediaElement, T>(
  element: E,
  file: Blob,
  timeoutMs: number,
  read: (element: E, signal: AbortSignal) => Promise<T | null>,
): Promise<T | null> {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), timeoutMs)
  const url = URL.createObjectURL(file)
  try {
    element.muted = true
    element.preload = 'auto'
    const loaded = once(element, ['loadedmetadata'], controller.signal)
    element.src = url
    await loaded
    return await read(element, controller.signal)
  } catch {
    return null
  } finally {
    clearTimeout(timer)
    controller.abort()
    element.removeAttribute('src')
    try {
      // Releases the decoder and the resource, per the HTML spec's advice
      // for emptying a media element.
      element.load()
    } catch {
      // A test double, or an engine that throws on an empty load.
    }
    URL.revokeObjectURL(url)
  }
}

/**
 * Whether RGBA pixels are one flat colour: solid black, or an undrawn
 * (transparent) canvas, which JPEG would paint black. Samples at most
 * [`BLANK_SAMPLES`] pixels, so a full 800 × 600 frame costs little.
 */
export function isBlankFrame(data: ArrayLike<number>): boolean {
  const pixels = Math.floor(data.length / 4)
  if (pixels === 0) return true
  const step = Math.max(1, Math.floor(pixels / BLANK_SAMPLES))
  let min = 255
  let max = 0
  for (let pixel = 0; pixel < pixels; pixel += step) {
    const i = pixel * 4
    const alpha = data[i + 3]! / 255
    const luma =
      (0.299 * data[i]! + 0.587 * data[i + 1]! + 0.114 * data[i + 2]!) * alpha
    if (luma < min) min = luma
    if (luma > max) max = luma
    if (max - min > BLANK_LUMA_SPREAD) return false
  }
  return true
}

/**
 * Resolve after `ms`, when `signal` aborts, or when `start` calls its
 * `done`, whichever comes first. It never rejects, so nothing is left to
 * reject unhandled once the probe's budget aborts; callers check the signal.
 */
function settleWithin(
  ms: number,
  signal: AbortSignal,
  start?: (done: () => void) => void,
): Promise<void> {
  return new Promise((resolve) => {
    const done = () => {
      clearTimeout(timer)
      signal.removeEventListener('abort', done)
      resolve()
    }
    const timer = setTimeout(done, ms)
    signal.addEventListener('abort', done)
    try {
      start?.(done)
    } catch {
      // An engine without the API, or one that throws: the timer decides.
    }
  })
}

function throwIfAborted(signal: AbortSignal): void {
  if (signal.aborted) throw new Error('aborted')
}

/**
 * Wait for the browser to present the current frame, where it says so
 * (`requestVideoFrameCallback`, Safari 15.4+), for at most `waitMs`.
 */
async function presentedFrame(
  video: HTMLVideoElement,
  signal: AbortSignal,
  waitMs: number,
): Promise<void> {
  await settleWithin(waitMs, signal, (done) => {
    if (typeof video.requestVideoFrameCallback === 'function')
      video.requestVideoFrameCallback(() => done())
  })
  throwIfAborted(signal)
}

/**
 * Nudge a frame out of a decoder that drew nothing: play muted for a moment,
 * then pause. iOS allows a muted inline video to play without a gesture; a
 * refusal is ignored, and the next draw simply tries again.
 */
async function playBriefly(
  video: HTMLVideoElement,
  signal: AbortSignal,
  waitMs: number,
): Promise<void> {
  try {
    await settleWithin(waitMs, signal, (done) => {
      void Promise.resolve(video.play()).then(done, done)
    })
    throwIfAborted(signal)
    await presentedFrame(video, signal, waitMs)
  } finally {
    video.pause()
  }
}

/**
 * Draw the element's current frame into `context`, and say whether it holds
 * a picture. Three tries: after the browser presents the frame, again after
 * playing for a moment, and once more after another frame.
 */
async function drawFrame(
  video: HTMLVideoElement,
  context: CanvasRenderingContext2D,
  width: number,
  height: number,
  signal: AbortSignal,
  waitMs: number,
): Promise<boolean> {
  const nudges = [presentedFrame, playBriefly, presentedFrame]
  for (const nudge of nudges) {
    await nudge(video, signal, waitMs)
    context.drawImage(video, 0, 0, width, height)
    let blank: boolean
    try {
      blank = isBlankFrame(context.getImageData(0, 0, width, height).data)
    } catch {
      // A canvas the browser will not read back cannot be checked; it holds
      // whatever was drawn, as it did before this check existed.
      return true
    }
    if (!blank) return true
  }
  return false
}

/**
 * Draw the element's current frame scaled into § 2's box, as a JPEG, or
 * `null` when it cannot be drawn or stays blank.
 */
async function encodePoster(
  video: HTMLVideoElement,
  width: number,
  height: number,
  createCanvas: () => HTMLCanvasElement,
  signal: AbortSignal,
  waitMs: number,
): Promise<Blob | null> {
  const size = fitWithin(width, height)
  const canvas = createCanvas()
  canvas.width = size.width
  canvas.height = size.height
  const context = canvas.getContext('2d')
  if (context === null) return null
  if (
    !(await drawFrame(video, context, size.width, size.height, signal, waitMs))
  )
    return null
  const blob = await abortable(
    new Promise<Blob | null>((resolve) =>
      canvas.toBlob(resolve, 'image/jpeg', POSTER_JPEG_QUALITY),
    ),
    signal,
  )
  // A browser that cannot encode JPEG falls back to PNG; the server accepts
  // that too and re-encodes it, so only an empty or oversized result fails.
  if (blob === null || blob.size === 0 || blob.size > MAX_POSTER_BYTES)
    return null
  return blob
}

/**
 * A video's dimensions, duration and poster frame, or `null` when the browser
 * could not decode it in time: ADR 0142 § 7 reports nothing it might have
 * guessed wrong for such a video. A decoded video whose frame draws blank, or
 * will not encode, keeps its dimensions and duration and gets no poster.
 */
export function probeVideo(
  file: Blob,
  deps: ProbeDeps = {},
): Promise<MediaProbe | null> {
  const video = (deps.createVideo ?? (() => document.createElement('video')))()
  // iOS Safari loads an off-DOM video only when it may play inline.
  video.playsInline = true
  return withLoaded(
    video,
    file,
    deps.timeoutMs ?? PROBE_TIMEOUT_MS,
    async (element, signal) => {
      const width = element.videoWidth
      const height = element.videoHeight
      // An audio-only file, or a video track the browser cannot decode,
      // loads with no picture.
      if (!dimension(width) || !dimension(height)) return null
      const duration = durationMs(element.duration)
      const seeked = once(element, ['seeked'], signal)
      element.currentTime = posterFrameTime(element.duration)
      await seeked
      if (element.readyState < HAVE_CURRENT_DATA)
        await once(element, ['loadeddata', 'canplay'], signal)
      const poster = await encodePoster(
        element,
        width,
        height,
        deps.createCanvas ?? (() => document.createElement('canvas')),
        signal,
        deps.frameWaitMs ?? FRAME_WAIT_MS,
      )
      return {
        info: {
          width,
          height,
          ...(duration === undefined ? {} : { duration_ms: duration }),
        },
        poster,
      }
    },
  )
}

/** An audio file's duration, or `null` when the browser could not read it. */
export function probeAudio(
  file: Blob,
  deps: ProbeDeps = {},
): Promise<MediaProbe | null> {
  const audio = (deps.createAudio ?? (() => document.createElement('audio')))()
  return withLoaded(
    audio,
    file,
    deps.timeoutMs ?? PROBE_TIMEOUT_MS,
    (element) => {
      const duration = durationMs(element.duration)
      return Promise.resolve(
        duration === undefined
          ? null
          : { info: { duration_ms: duration }, poster: null },
      )
    },
  )
}
