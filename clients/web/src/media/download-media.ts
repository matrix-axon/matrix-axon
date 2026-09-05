import { browserPlatform, type Platform, type SaveOutcome } from '../platform'
import type { MediaService } from './media-service'
import type { ParsedMedia } from './parse-media'

/**
 * How a save ended. The share/anchor/dialog mechanics live in the platform
 * seam (ADR 0102 § 2) — this module only decides *what* to save.
 */
export type DownloadOutcome = SaveOutcome

/**
 * Whether this media can be saved at all: only an `mxc://` object has bytes
 * the proxy can hand back.
 */
export function isDownloadable(media: ParsedMedia): boolean {
  return media.url !== null && media.url.startsWith('mxc://')
}

/**
 * Save a piece of media to the device.
 *
 * Deliberately re-fetches instead of reusing an object URL the caller may
 * already be displaying: those are owned by `useMediaBlob`'s reference
 * counting and can be evicted by the LRU while the browser is still writing
 * the file. The blob acquired here is owned here.
 *
 * How the bytes reach the device is the platform's business. A browser offers
 * the share sheet, then a transient `<a download>`; a packaged build asks the
 * OS for a path and writes the file, because that anchor is inert from a
 * custom scheme and would silently do nothing.
 */
export async function downloadMedia(
  service: MediaService,
  accountId: string,
  media: ParsedMedia,
  platform: Pick<Platform, 'saveFile'> = browserPlatform(),
): Promise<DownloadOutcome> {
  if (media.url === null) {
    return 'failed'
  }

  const blob = await service.fetchBlob(accountId, media.url)
  if (!blob.ok) {
    return 'failed'
  }
  return platform.saveFile({
    blob: blob.blob,
    filename: media.filename,
    mimetype: media.mimetype,
  })
}
