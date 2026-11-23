import { fileFromPath } from '../media/dropped-file'
import { SNIFF_HEAD_BYTES, sniffImageFormat } from '../media/sniff'
import { isTauriRuntime } from './index'

export function supportsNativeImagePaste(): boolean {
  return __AXON_TAURI_PLATFORM__ === 'linux' && isTauriRuntime()
}

/** On X11, getData can return text even when types is empty. */
export function canFallBackToClipboardImage(
  clipboard: Pick<DataTransfer, 'types'>,
  text: string,
): boolean {
  return (
    text === '' &&
    clipboard.types.every(
      (type) => type === 'Files' || type.startsWith('image/'),
    )
  )
}

/** GTK sends encoded raster bytes directly over IPC. */
export async function readClipboardImage(): Promise<File | null> {
  const { invoke } = await import('@tauri-apps/api/core')
  const bytes = await invoke<ArrayBuffer>('read_clipboard_image')
  const format = sniffImageFormat(
    new Uint8Array(bytes, 0, Math.min(bytes.byteLength, SNIFF_HEAD_BYTES)),
  )
  switch (format) {
    case 'PNG':
    case 'JPEG':
    case 'WebP':
    case 'GIF':
    case 'BMP':
    case 'TIFF':
    case 'AVIF':
      return fileFromPath(`clipboard.${format.toLowerCase()}`, bytes)
    default:
      return null
  }
}
