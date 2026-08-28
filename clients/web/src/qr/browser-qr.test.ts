import { afterEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
  jsQr: vi.fn(),
  toCanvas: vi.fn(),
}))

vi.mock('jsqr', () => ({ default: mocks.jsQr }))
vi.mock('qrcode', () => ({ toCanvas: mocks.toCanvas }))

import {
  createRetryableLazyLoader,
  createBrowserQrAdapter,
  decodeUnpaddedBase64,
  encodeUnpaddedBase64,
  MAX_QR_IMAGE_BYTES,
} from './browser-qr'

const originalGetContext = HTMLCanvasElement.prototype.getContext

afterEach(() => {
  vi.useRealTimers()
  vi.unstubAllGlobals()
  mocks.jsQr.mockReset()
  mocks.toCanvas.mockReset()
  HTMLCanvasElement.prototype.getContext = originalGetContext
})

function mockCanvas() {
  const drawImage = vi.fn()
  const getImageData = vi.fn(() => ({
    data: new Uint8ClampedArray(4),
    width: 1,
    height: 1,
  }))
  HTMLCanvasElement.prototype.getContext = vi.fn(() => ({
    drawImage,
    getImageData,
  })) as unknown as typeof HTMLCanvasElement.prototype.getContext
  return { drawImage, getImageData }
}

describe('browser QR adapter', () => {
  it('reuses a pending lazy load but retries after rejection', async () => {
    const load = vi
      .fn<() => Promise<string>>()
      .mockRejectedValueOnce(new Error('chunk offline'))
      .mockResolvedValue('decoder')
    const retryableLoad = createRetryableLazyLoader(load)

    const first = retryableLoad()
    expect(retryableLoad()).toBe(first)
    await expect(first).rejects.toThrow('chunk offline')
    await expect(retryableLoad()).resolves.toBe('decoder')
    expect(load).toHaveBeenCalledTimes(2)
  })

  it('round-trips padded, unpadded, URL-safe, and arbitrary binary bytes', () => {
    const bytes = Uint8Array.from([0, 1, 2, 127, 128, 254, 255])
    const unpadded = encodeUnpaddedBase64(bytes)

    expect(unpadded).not.toContain('=')
    expect(decodeUnpaddedBase64(unpadded)).toEqual(bytes)
    expect(decodeUnpaddedBase64(`${unpadded}==`)).toEqual(bytes)
    expect(decodeUnpaddedBase64('-_8')).toEqual(Uint8Array.from([251, 255]))
    expect(() => decodeUnpaddedBase64('not base64!')).toThrow(/base64/)
  })

  it('renders raw bytes in QR byte mode through the lazy library', async () => {
    mocks.toCanvas.mockResolvedValue(undefined)
    const canvas = document.createElement('canvas')
    const bytes = Uint8Array.from([0, 255, 32, 128])

    await createBrowserQrAdapter().render(canvas, bytes)

    expect(mocks.toCanvas).toHaveBeenCalledWith(
      canvas,
      [{ data: bytes, mode: 'byte' }],
      expect.objectContaining({ errorCorrectionLevel: 'L' }),
    )
  })

  it('bounds uploaded images, decodes one binary result, and releases resources', async () => {
    const { drawImage } = mockCanvas()
    const close = vi.fn()
    vi.stubGlobal(
      'createImageBitmap',
      vi.fn().mockResolvedValue({ width: 4096, height: 2048, close }),
    )
    mocks.jsQr.mockReturnValue({ binaryData: [0, 1, 254, 255] })
    const file = new File([new Uint8Array([1, 2, 3])], 'qr.png', {
      type: 'image/png',
    })

    await expect(createBrowserQrAdapter().scanImage(file)).resolves.toEqual(
      Uint8Array.from([0, 1, 254, 255]),
    )
    expect(drawImage).toHaveBeenCalledWith(
      expect.objectContaining({ width: 4096 }),
      0,
      0,
      2048,
      1024,
    )
    expect(close).toHaveBeenCalledOnce()
  })

  it('rejects oversized, non-image, and undecodable uploads', async () => {
    const adapter = createBrowserQrAdapter()
    const oversized = new File(
      [new Uint8Array(MAX_QR_IMAGE_BYTES + 1)],
      'qr.png',
      { type: 'image/png' },
    )
    await expect(adapter.scanImage(oversized)).rejects.toThrow(
      /smaller than 8 MB/,
    )
    await expect(
      adapter.scanImage(new File(['text'], 'qr.txt', { type: 'text/plain' })),
    ).rejects.toThrow(/image file/)

    mockCanvas()
    const close = vi.fn()
    vi.stubGlobal(
      'createImageBitmap',
      vi.fn().mockResolvedValue({ width: 20, height: 20, close }),
    )
    mocks.jsQr.mockReturnValue(null)
    await expect(
      adapter.scanImage(new File(['image'], 'qr.png', { type: 'image/png' })),
    ).rejects.toThrow(/No QR code/)
    expect(close).toHaveBeenCalledOnce()
  })

  it('surfaces camera permission failure', async () => {
    const getUserMedia = vi
      .fn()
      .mockRejectedValue(new Error('permission denied'))
    vi.stubGlobal('navigator', {
      mediaDevices: { getUserMedia },
    })

    await expect(
      createBrowserQrAdapter().startCamera(
        document.createElement('video'),
        vi.fn(),
        vi.fn(),
      ),
    ).rejects.toThrow('permission denied')
  })

  it('enumerates selectable cameras and observes device changes', async () => {
    const enumerateDevices = vi.fn().mockResolvedValue([
      { kind: 'audioinput', deviceId: 'mic', label: 'Microphone' },
      { kind: 'videoinput', deviceId: 'front', label: 'Front camera' },
      { kind: 'videoinput', deviceId: 'rear', label: '' },
      { kind: 'videoinput', deviceId: '', label: '' },
    ])
    const listeners = new Set<EventListenerOrEventListenerObject>()
    const addEventListener = vi.fn(
      (_type: string, listener: EventListenerOrEventListenerObject) =>
        listeners.add(listener),
    )
    const removeEventListener = vi.fn(
      (_type: string, listener: EventListenerOrEventListenerObject) =>
        listeners.delete(listener),
    )
    vi.stubGlobal('navigator', {
      mediaDevices: {
        enumerateDevices,
        addEventListener,
        removeEventListener,
      },
    })
    const adapter = createBrowserQrAdapter()

    await expect(adapter.listCameras()).resolves.toEqual([
      { deviceId: 'front', label: 'Front camera' },
      { deviceId: 'rear', label: 'Camera 2' },
    ])
    const changed = vi.fn()
    const unwatch = adapter.watchCameras(changed)
    for (const listener of listeners) {
      if (typeof listener === 'function') {
        listener(new Event('devicechange'))
      } else {
        listener.handleEvent(new Event('devicechange'))
      }
    }
    expect(changed).toHaveBeenCalledOnce()

    unwatch()
    expect(removeEventListener).toHaveBeenCalledWith('devicechange', changed)
    expect(listeners).toHaveLength(0)
  })

  it('latches the first camera result and releases tracks, timer, and canvas', async () => {
    vi.useFakeTimers()
    mockCanvas()
    mocks.jsQr.mockReturnValue({ binaryData: [9, 8, 7] })
    const stopTrack = vi.fn()
    const track = {
      stop: stopTrack,
      getSettings: () => ({ deviceId: 'rear' }),
    }
    const stream = {
      getTracks: () => [track],
      getVideoTracks: () => [track],
    } as unknown as MediaStream
    const getUserMedia = vi.fn().mockResolvedValue(stream)
    vi.stubGlobal('navigator', {
      mediaDevices: { getUserMedia },
    })
    const video = document.createElement('video')
    Object.defineProperties(video, {
      play: { configurable: true, value: vi.fn().mockResolvedValue(undefined) },
      pause: { configurable: true, value: vi.fn() },
      videoWidth: { configurable: true, value: 640 },
      videoHeight: { configurable: true, value: 480 },
      srcObject: { configurable: true, writable: true, value: null },
    })
    const onResult = vi.fn()

    const session = await createBrowserQrAdapter().startCamera(
      video,
      onResult,
      vi.fn(),
    )
    expect(getUserMedia).toHaveBeenCalledWith({
      video: { facingMode: { ideal: 'environment' } },
      audio: false,
    })
    expect(session.deviceId).toBe('rear')
    await vi.advanceTimersByTimeAsync(250)
    await vi.runAllTicks()

    expect(onResult).toHaveBeenCalledOnce()
    expect(onResult).toHaveBeenCalledWith(Uint8Array.from([9, 8, 7]))
    expect(stopTrack).toHaveBeenCalledOnce()
    expect(video.srcObject).toBeNull()

    await vi.advanceTimersByTimeAsync(1_000)
    expect(onResult).toHaveBeenCalledOnce()
    session.stop()
    expect(stopTrack).toHaveBeenCalledOnce()
  })

  it('opens an explicitly selected camera and reports its active device id', async () => {
    const stopTrack = vi.fn()
    const track = {
      stop: stopTrack,
      getSettings: () => ({ deviceId: 'usb-camera' }),
    }
    const stream = {
      getTracks: () => [track],
      getVideoTracks: () => [track],
    } as unknown as MediaStream
    const getUserMedia = vi.fn().mockResolvedValue(stream)
    vi.stubGlobal('navigator', { mediaDevices: { getUserMedia } })
    const video = document.createElement('video')
    Object.defineProperties(video, {
      play: { configurable: true, value: vi.fn().mockResolvedValue(undefined) },
      pause: { configurable: true, value: vi.fn() },
      srcObject: { configurable: true, writable: true, value: null },
    })

    const session = await createBrowserQrAdapter().startCamera(
      video,
      vi.fn(),
      vi.fn(),
      'usb-camera',
    )

    expect(getUserMedia).toHaveBeenCalledWith({
      video: { deviceId: { exact: 'usb-camera' } },
      audio: false,
    })
    expect(session.deviceId).toBe('usb-camera')
    session.stop()
    expect(stopTrack).toHaveBeenCalledOnce()
  })
})
