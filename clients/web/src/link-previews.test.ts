import { describe, expect, it } from 'vitest'
import { parseLinkPreviews } from './link-previews'

const url = 'https://example.org/article'
const entry = { matched_url: url, 'og:title': 'Article' }
const bundle = (entries: unknown) => ({ 'com.beeper.linkpreviews': entries })

describe('parseLinkPreviews', () => {
  it('accepts standard metadata and prefers it over the compatibility field', () => {
    expect(
      parseLinkPreviews(
        {
          ...bundle([entry]),
          'm.url_previews': [
            { 'matrix:matched_url': url, 'og:title': 'Standard' },
          ],
        },
        url,
      )[0]?.title,
    ).toBe('Standard')
    for (const standard of [[], null, {}, 'invalid']) {
      expect(
        parseLinkPreviews(
          { ...bundle([entry]), 'm.url_previews': standard },
          url,
        ),
      ).toEqual([])
    }
  })

  it.each([
    null,
    undefined,
    [],
    false,
    42,
    'text',
    {},
    bundle({}),
    bundle([null, [], {}, 1]),
  ])('ignores malformed content %j', (content) => {
    expect(parseLinkPreviews(content, url)).toEqual([])
  })

  it('requires an actual matching body URL, not a substring of a different URL', () => {
    for (const body of [
      null,
      '',
      'no link',
      `${url}/different`,
      `https://elsewhere.test/?next=${url}`,
    ]) {
      expect(parseLinkPreviews(bundle([entry]), body)).toEqual([])
    }
    expect(parseLinkPreviews(bundle([entry]), `See ${url}.`)).toHaveLength(1)
  })

  it.each([
    'javascript:alert(1)',
    'data:text/html,x',
    'file:///etc/passwd',
    '//example.org',
    'https://user:password@example.org',
    'https://[bad',
  ])('rejects unsafe or malformed destination %s', (unsafe) => {
    expect(
      parseLinkPreviews(bundle([{ ...entry, matched_url: unsafe }]), unsafe),
    ).toEqual([])
  })

  it('deduplicates previews and ignores entries without usable text', () => {
    expect(
      parseLinkPreviews(
        bundle([entry, entry, { matched_url: url, 'og:title': {} }]),
        url,
      ),
    ).toHaveLength(1)
    expect(
      parseLinkPreviews(
        bundle([
          { matched_url: url, 'og:image': 'mxc://hs/image' },
          { matched_url: url },
        ]),
        url,
      ),
    ).toEqual([])
    expect(
      parseLinkPreviews(
        bundle([{ matched_url: url, 'og:description': 'Description only' }]),
        url,
      )[0]?.description,
    ).toBe('Description only')
  })

  it('bounds text, URL and body lengths, scanned entries, and returned cards', () => {
    const parsed = parseLinkPreviews(
      bundle([
        {
          ...entry,
          'og:title': 't'.repeat(500),
          'og:description': 'd'.repeat(2000),
          'og:site_name': 's'.repeat(500),
        },
      ]),
      url,
    )[0]!
    expect(parsed.title).toHaveLength(300)
    expect(parsed.description).toHaveLength(1000)
    expect(parsed.siteName).toHaveLength(120)
    expect(
      parseLinkPreviews(bundle([entry]), `${url} ${'x'.repeat(65_536)}`),
    ).toEqual([])
    const hugeUrl = `https://example.org/${'x'.repeat(4096)}`
    expect(
      parseLinkPreviews(bundle([{ ...entry, matched_url: hugeUrl }]), hugeUrl),
    ).toEqual([])
    expect(
      parseLinkPreviews(bundle([...Array<null>(32).fill(null), entry]), url),
    ).toEqual([])
    const entries = Array.from({ length: 20 }, (_, i) => ({
      ...entry,
      matched_url: `${url}/${i}`,
    }))
    expect(
      parseLinkPreviews(
        bundle(entries),
        entries.map((e) => e.matched_url).join(' '),
      ),
    ).toHaveLength(8)
  })
})
