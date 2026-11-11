import { describe, expect, it } from 'vitest'
import {
  linkIdentity,
  nativePreviewLinks,
  PREVIEW_FIELD_LIMITS,
  previewField,
} from './preview-links'
import cases from './incoming-share/fixtures/device-preview-urls.json'
import identity from './fixtures/link-identity.json'

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

describe('link identity (shared with the server)', () => {
  it.each(identity.cases)('$name', ({ url, href }) => {
    expect(linkIdentity(url)).toBe(href)
  })
  it('gives two spellings of one destination one identity', () => {
    expect(linkIdentity('https://EXAMPLE.org:443')).toBe(
      linkIdentity('https://example.org/'),
    )
    expect(linkIdentity('https://example.org/a#x')).not.toBe(
      linkIdentity('https://example.org/a'),
    )
    expect(linkIdentity('https://example.org/a/')).not.toBe(
      linkIdentity('https://example.org/a'),
    )
  })
  it('has no identity for text that does not parse', () => {
    expect(linkIdentity('https://')).toBeNull()
  })
})

describe('preview field normalization', () => {
  it('trims Foundation whitespace, including U+200B and no-break spaces', () => {
    expect(previewField('\u200B\u00A0 Title \n\u2003', 300)).toBe('Title')
  })
  it('keeps inner whitespace', () => {
    expect(previewField(' a\u00A0 b ', 300)).toBe('a\u00A0 b')
  })
  it('drops an empty value', () => {
    expect(previewField(' \u200B ', 300)).toBeNull()
    expect(previewField('', 300)).toBeNull()
  })
  it('drops a value over the limit before trimming, never truncating it', () => {
    expect(previewField(`${'x'.repeat(300)} `, 300)).toBeNull()
    expect(previewField('x'.repeat(300), 300)).toBe('x'.repeat(300))
  })
  it('counts UTF-16 code units', () => {
    // One astral character is two code units.
    expect(previewField('😀'.repeat(60), PREVIEW_FIELD_LIMITS.siteName)).toBe(
      '😀'.repeat(60),
    )
    expect(
      previewField('😀'.repeat(61), PREVIEW_FIELD_LIMITS.siteName),
    ).toBeNull()
  })
  it('drops a value that is not a string', () => {
    expect(previewField(42, 300)).toBeNull()
    expect(previewField(null, 300)).toBeNull()
  })
})
