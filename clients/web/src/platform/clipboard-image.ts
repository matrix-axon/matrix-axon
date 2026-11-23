import { fileFromPath } from '../media/dropped-file'
import { SNIFF_HEAD_BYTES, sniffImageFormat } from '../media/sniff'
import { isTauriRuntime } from './index'

export function supportsNativeImagePaste(): boolean {
  return __AXON_TAURI_PLATFORM__ === 'linux' && isTauriRuntime()
}

export function canFallBackToClipboardImage(
  clipboard: Pick<DataTransfer, 'types' | 'getData'>,
  text: string,
): boolean {
  // On X11, getData can return text even when types is empty.
  if (
    text === '' &&
    clipboard.types.every(
      (type) => type === 'Files' || type.startsWith('image/'),
    )
  ) {
    return true
  }

  // Older WebKitGTK exposes Copy Image as a URI and an HTML image fragment.
  // GTK still has the raster bytes. Copy Image Address produces a link
  // fragment, and rich selections can carry both text and images.
  if (
    !clipboard.types.includes('text/uri-list') ||
    !clipboard.types.includes('text/html')
  ) {
    return false
  }
  const uri = clipboard.getData('text/uri-list').trim()
  if (uri === '' || /\s/.test(uri) || (text !== '' && text.trim() !== uri)) {
    return false
  }

  const html = clipboard.getData('text/html')
  // Parse the small fragment in an inert template without loading its image.
  if (html.length > 16 * 1024) return false
  const template = document.createElement('template')
  template.innerHTML = html
  const { content } = template
  const image = content.firstElementChild
  if (
    image?.localName !== 'img' ||
    content.childElementCount !== 1 ||
    content.textContent?.trim()
  ) {
    return false
  }
  const src = image.getAttribute('src')
  // WebKitGTK strips the blob src from the HTML it exposes to the paste event.
  return src === uri || (src === null && uri.startsWith('blob:'))
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
