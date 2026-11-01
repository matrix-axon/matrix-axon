import { matchUrls } from '../html/linkify'

/** The portable, escaped ASCII subset accepted by Foundation on iOS 15+.
 * Other URLs remain ordinary message text; ineligible metadata is optional. */
export function nativePreviewLinks(text: string): string[] {
  return matchUrls(text)
    .map(({ url }) => url)
    .filter((raw) => {
      if (
        raw.length > 4096 ||
        /%(?![0-9a-f]{2})/i.test(raw) ||
        !/^[A-Za-z0-9\-._~:/?#[\]@!$&'()*+,;=%]+$/.test(raw)
      )
        return false
      try {
        const url = new URL(raw)
        return (
          url.protocol === 'https:' &&
          url.hostname !== '' &&
          url.username === '' &&
          url.password === ''
        )
      } catch {
        return false
      }
    })
}
