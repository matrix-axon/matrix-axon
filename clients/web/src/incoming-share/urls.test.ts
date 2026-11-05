import { describe, expect, it } from 'vitest'
import { nativePreviewLinks } from './urls'
import cases from './fixtures/device-preview-urls.json'

describe('portable native preview URLs', () => {
  it.each(cases)(
    'matches native static eligibility for $url',
    ({ url, eligible }) => {
      expect(nativePreviewLinks(`Keep this link ${url}`).includes(url)).toBe(
        eligible,
      )
    },
  )
  it.each([
    'http://example.org/article',
    'https://example.org/é',
    'https://example.org/a|b',
    'https://example.org/{a}',
    'https://example.org/a^b',
    'https://u:p@example.org/',
    'https://example.org/%XX',
  ])('keeps unsupported URL %s as ordinary text', (url) => {
    expect(nativePreviewLinks(`Keep this link ${url}`)).toEqual([])
  })
  it('keeps escaped paths and substantive text unchanged', () => {
    expect(
      nativePreviewLinks('A note https://example.org/%C3%A9?q=a%7Cb.'),
    ).toEqual(['https://example.org/%C3%A9?q=a%7Cb'])
  })
})
