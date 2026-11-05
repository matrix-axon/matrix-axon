import { matchUrls } from '../html/linkify'

/** Static URL eligibility shared with the native restricted preview reader.
 * DNS and every redirect/image address still require native validation.
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
        const host = url.hostname.toLowerCase().replace(/\.$/, '')
        const labels = host.split('.')
        return (
          host.length <= 253 &&
          labels.length > 1 &&
          /^[a-z0-9.-]+$/.test(host) &&
          labels.every(
            (label) =>
              label.length > 0 &&
              label.length <= 63 &&
              !label.startsWith('-') &&
              !label.endsWith('-') &&
              !label.startsWith('xn--'),
          ) &&
          !/^\d+$/.test(labels.at(-1)!) &&
          ![
            'local',
            'localhost',
            'home',
            'lan',
            'corp',
            'internal',
            'localdomain',
            'intranet',
            'private',
            'onion',
            'i2p',
            'invalid',
            'test',
            'example',
            'alt',
            'arpa',
          ].includes(labels.at(-1)!) &&
          (url.port === '' ||
            (Number(url.port) >= 1 && Number(url.port) <= 65535)) &&
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
