/**
 * Turning an OS file path into the `File` the rest of the client expects.
 *
 * Only the native drag-drop path needs this (`platform/tauri.ts`). Every other
 * way a file enters the app — the paperclip's `<input type="file">`, a paste,
 * a browser drop — already produces a `File`, with a name and a type the
 * platform filled in. A shell drop produces a *path* and nothing else, so both
 * have to be recovered here.
 */

/**
 * Media types by extension.
 *
 * A guess, and deliberately a short one. `file.type` is not decoration: it
 * decides `m.image` versus `m.file` (`media/media-service.ts`), and it becomes
 * the upload's `Content-Type` and the event's `mimetype`
 * (`media/event-media.ts`). Getting it wrong sends an image that renders as a
 * download link for every other client in the room, forever.
 *
 * So this covers what people actually drag onto a chat window and stops.
 * Anything unlisted gets `''`, which is exactly what a browser reports for a
 * file it cannot type, and which the send path already handles by omitting
 * `mimetype` rather than asserting a wrong one.
 */
const MEDIA_TYPES: Readonly<Record<string, string>> = {
  apng: 'image/apng',
  avif: 'image/avif',
  bmp: 'image/bmp',
  gif: 'image/gif',
  heic: 'image/heic',
  heif: 'image/heif',
  ico: 'image/vnd.microsoft.icon',
  jfif: 'image/jpeg',
  jpe: 'image/jpeg',
  jpeg: 'image/jpeg',
  jpg: 'image/jpeg',
  png: 'image/png',
  svg: 'image/svg+xml',
  tif: 'image/tiff',
  tiff: 'image/tiff',
  webp: 'image/webp',

  aac: 'audio/aac',
  flac: 'audio/flac',
  m4a: 'audio/mp4',
  mp3: 'audio/mpeg',
  oga: 'audio/ogg',
  ogg: 'audio/ogg',
  opus: 'audio/ogg',
  wav: 'audio/wav',

  avi: 'video/x-msvideo',
  m4v: 'video/mp4',
  mkv: 'video/x-matroska',
  mov: 'video/quicktime',
  mp4: 'video/mp4',
  ogv: 'video/ogg',
  webm: 'video/webm',

  csv: 'text/csv',
  json: 'application/json',
  md: 'text/markdown',
  pdf: 'application/pdf',
  txt: 'text/plain',
  zip: 'application/zip',
}

/**
 * The last path segment, for either separator.
 *
 * Both are checked on every platform rather than branching on the host: a
 * Windows path can reach a Linux build through a shared volume or a test, and
 * a name that still had `C:\Users\…` in it would be shown to the user and sent
 * to the room as the filename.
 */
export function basename(path: string): string {
  const cut = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'))
  return cut === -1 ? path : path.slice(cut + 1)
}

/** The guessed media type for a path, or `''` when there is no good guess. */
export function mediaTypeForPath(path: string): string {
  const name = basename(path)
  const dot = name.lastIndexOf('.')
  // `> 0`, not `>= 0`: a leading dot is a hidden file, not an extension, so
  // `.gitignore` must not be typed as whatever `gitignore` would map to.
  if (dot <= 0) {
    return ''
  }
  return MEDIA_TYPES[name.slice(dot + 1).toLowerCase()] ?? ''
}

/** Assemble the `File` for a path whose bytes have already been read. */
export function fileFromPath(path: string, bytes: ArrayBuffer): File {
  return new File([bytes], basename(path), { type: mediaTypeForPath(path) })
}
