export const MAX_QR_IMAGE_BYTES = 8 * 1024 * 1024
export const MAX_QR_SCAN_DIMENSION = 2048
const CAMERA_SCAN_INTERVAL_MS = 250
type JsQrDecoder = (typeof import('jsqr'))['default']

export function createRetryableLazyLoader<T>(
  load: () => Promise<T>,
): () => Promise<T> {
  let pending: Promise<T> | null = null
  return () => {
    pending ??= load().catch((cause: unknown) => {
      pending = null
      throw cause
    })
    return pending
  }
}

const loadJsQrDecoder = createRetryableLazyLoader<JsQrDecoder>(() =>
  import('jsqr').then(({ default: decoder }) => decoder),
)

export interface QrCameraDevice {
  deviceId: string
  label: string
}

export interface QrCameraSession {
  deviceId: string | null
  stop(): void
}

export interface BrowserQrAdapter {
  decodeBase64(value: string): Uint8Array
  encodeBase64(bytes: Uint8Array): string
  render(canvas: HTMLCanvasElement, bytes: Uint8Array): Promise<void>
  scanImage(file: File): Promise<Uint8Array>
  listCameras(): Promise<QrCameraDevice[]>
  watchCameras(onChange: () => void): () => void
  startCamera(
    video: HTMLVideoElement,
    onResult: (bytes: Uint8Array) => void,
    onError: (message: string) => void,
    deviceId?: string,
  ): Promise<QrCameraSession>
}

function normalizedBase64(value: string): string {
  if (!/^[A-Za-z0-9+/_-]*={0,2}$/.test(value)) {
    throw new Error('QR data is not valid base64')
  }
  const unpadded = value
    .replace(/=+$/, '')
    .replace(/-/g, '+')
    .replace(/_/g, '/')
  const remainder = unpadded.length % 4
  if (remainder === 1) {
    throw new Error('QR data is not valid base64')
  }
  return unpadded + (remainder === 0 ? '' : '='.repeat(4 - remainder))
}

export function decodeUnpaddedBase64(value: string): Uint8Array {
  let decoded: string
  try {
    decoded = atob(normalizedBase64(value))
  } catch {
    throw new Error('QR data is not valid base64')
  }
  return Uint8Array.from(decoded, (character) => character.charCodeAt(0))
}

export function encodeUnpaddedBase64(bytes: Uint8Array): string {
  let binary = ''
  for (const byte of bytes) {
    binary += String.fromCharCode(byte)
  }
  return btoa(binary).replace(/=+$/, '')
}

function boundedDimensions(width: number, height: number): [number, number] {
  if (
    !Number.isFinite(width) ||
    !Number.isFinite(height) ||
    width <= 0 ||
    height <= 0
  ) {
    throw new Error('The selected image has invalid dimensions.')
  }
  const scale = Math.min(1, MAX_QR_SCAN_DIMENSION / Math.max(width, height))
  return [
    Math.max(1, Math.round(width * scale)),
    Math.max(1, Math.round(height * scale)),
  ]
}

function decodeCanvas(
  canvas: HTMLCanvasElement,
  jsQR: JsQrDecoder,
): Uint8Array | null {
  const context = canvas.getContext('2d', { willReadFrequently: true })
  if (context === null) {
    throw new Error('This browser cannot read QR images.')
  }
  const image = context.getImageData(0, 0, canvas.width, canvas.height)
  const result = jsQR(image.data, image.width, image.height, {
    inversionAttempts: 'attemptBoth',
  })
  return result === null ? null : Uint8Array.from(result.binaryData)
}

export function createBrowserQrAdapter(): BrowserQrAdapter {
  return {
    decodeBase64: decodeUnpaddedBase64,
    encodeBase64: encodeUnpaddedBase64,

    render: async (canvas, bytes) => {
      const { toCanvas } = await import('qrcode')
      await toCanvas(canvas, [{ data: bytes, mode: 'byte' }], {
        errorCorrectionLevel: 'L',
        margin: 2,
        width: 320,
      })
    },

    scanImage: async (file) => {
      if (file.size > MAX_QR_IMAGE_BYTES) {
        throw new Error('Choose an image smaller than 8 MB.')
      }
      if (!file.type.startsWith('image/')) {
        throw new Error('Choose an image file containing a QR code.')
      }
      let bitmap: ImageBitmap | null = null
      const canvas = document.createElement('canvas')
      try {
        bitmap = await createImageBitmap(file)
        const [width, height] = boundedDimensions(bitmap.width, bitmap.height)
        canvas.width = width
        canvas.height = height
        const context = canvas.getContext('2d', { willReadFrequently: true })
        if (context === null) {
          throw new Error('This browser cannot read QR images.')
        }
        context.drawImage(bitmap, 0, 0, width, height)
        const result = decodeCanvas(canvas, await loadJsQrDecoder())
        if (result === null) {
          throw new Error('No QR code was found in that image.')
        }
        return result
      } catch (cause) {
        if (cause instanceof Error) {
          throw cause
        }
        throw new Error('The selected image could not be decoded.', { cause })
      } finally {
        bitmap?.close()
        canvas.width = 0
        canvas.height = 0
      }
    },

    listCameras: async () => {
      if (navigator.mediaDevices?.enumerateDevices === undefined) {
        return []
      }
      const devices = await navigator.mediaDevices.enumerateDevices()
      return devices
        .filter(
          (device) => device.kind === 'videoinput' && device.deviceId !== '',
        )
        .map((device, index) => ({
          deviceId: device.deviceId,
          label: device.label.trim() || `Camera ${index + 1}`,
        }))
    },

    watchCameras: (onChange) => {
      const mediaDevices = navigator.mediaDevices
      if (mediaDevices?.addEventListener === undefined) {
        return () => {}
      }
      mediaDevices.addEventListener('devicechange', onChange)
      return () => mediaDevices.removeEventListener('devicechange', onChange)
    },

    startCamera: async (video, onResult, onError, deviceId) => {
      if (navigator.mediaDevices?.getUserMedia === undefined) {
        throw new Error('Camera access is unavailable in this browser.')
      }
      const stream = await navigator.mediaDevices.getUserMedia({
        video:
          deviceId === undefined
            ? { facingMode: { ideal: 'environment' } }
            : { deviceId: { exact: deviceId } },
        audio: false,
      })
      const activeDeviceId =
        stream.getVideoTracks()[0]?.getSettings().deviceId ?? deviceId ?? null
      const canvas = document.createElement('canvas')
      let timer: ReturnType<typeof setInterval> | null = null
      let stopped = false
      let decoding = false
      let latched = false
      const stop = () => {
        if (stopped) {
          return
        }
        stopped = true
        if (timer !== null) {
          clearInterval(timer)
          timer = null
        }
        for (const track of stream.getTracks()) {
          track.stop()
        }
        video.pause()
        video.srcObject = null
        canvas.width = 0
        canvas.height = 0
      }
      try {
        const jsQR = await loadJsQrDecoder()
        video.srcObject = stream
        await video.play()
        timer = setInterval(() => {
          if (
            stopped ||
            decoding ||
            latched ||
            video.videoWidth === 0 ||
            video.videoHeight === 0
          ) {
            return
          }
          decoding = true
          void (async () => {
            try {
              const [width, height] = boundedDimensions(
                video.videoWidth,
                video.videoHeight,
              )
              canvas.width = width
              canvas.height = height
              const context = canvas.getContext('2d', {
                willReadFrequently: true,
              })
              if (context === null) {
                throw new Error('This browser cannot read camera frames.')
              }
              context.drawImage(video, 0, 0, width, height)
              const result = decodeCanvas(canvas, jsQR)
              if (result !== null && !latched && !stopped) {
                latched = true
                stop()
                onResult(result)
              }
            } catch (cause) {
              if (!stopped) {
                onError(
                  cause instanceof Error
                    ? cause.message
                    : 'The camera frame could not be decoded.',
                )
              }
            } finally {
              decoding = false
            }
          })()
        }, CAMERA_SCAN_INTERVAL_MS)
        return { deviceId: activeDeviceId, stop }
      } catch (cause) {
        stop()
        throw cause instanceof Error
          ? cause
          : new Error('Camera access could not be started.')
      }
    },
  }
}
