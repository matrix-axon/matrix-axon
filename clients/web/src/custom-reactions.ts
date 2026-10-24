import { mxcToPath } from './media/parse-media'

/** Match the server's structural and byte-length limits for reaction keys. */
export function customReactionMxc(key: string): string | null {
  if (key.length > 2048 || new TextEncoder().encode(key).length > 2048) {
    return null
  }
  const path = mxcToPath(key)
  return path !== null && !path.mediaId.includes('/') ? key : null
}

export function reactionShortcode(shortcode: unknown): string | null {
  // Newer servers supply a name; older servers and old cached aggregates do
  // not. Treat even typed API strings as untrusted optional display text.
  if (
    typeof shortcode === 'string' &&
    shortcode.length <= 100 &&
    new TextEncoder().encode(shortcode).length <= 100 &&
    shortcode.trim().length > 0 &&
    !/\p{Cc}/u.test(shortcode)
  ) {
    return shortcode.trim()
  }
  return null
}

export function reactionLabel(key: string, shortcode?: unknown): string {
  return customReactionMxc(key) === null
    ? key
    : (reactionShortcode(shortcode) ?? 'Custom reaction')
}
