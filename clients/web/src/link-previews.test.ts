import { describe, expect, it } from 'vitest'
import { parseLinkPreviews } from './link-previews'
import { encryptedPreviewFile } from './test/link-preview'

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

  it('deduplicates previews and accepts images without text metadata', () => {
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
    ).toMatchObject([{ image: { mxc: 'mxc://hs/image', encrypted: false } }])
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

describe('preview image validation', () => {
  const parse = (fields: Record<string, unknown>) =>
    parseLinkPreviews(bundle([{ ...entry, ...fields }]), url)[0]!.image

  it.each(['matrix:image:encrypted', 'beeper:image:encryption'])(
    'accepts %s without retaining key material in the normalized shape',
    (field) => {
      expect(
        parse({ [field]: encryptedPreviewFile, 'og:image': 'mxc://hs/plain' }),
      ).toEqual({ mxc: encryptedPreviewFile.url, encrypted: true })
    },
  )

  it.each([
    'https://tracking.test/pixel',
    'data:image/png;base64,abc',
    'file:///local',
    'mxc:///image',
    'mxc://hs/',
    'mxc://hs/a/b',
    'mxc://user@hs/image',
    'mxc://hs/image?query',
    'mxc://hs/image#fragment',
    'mxc://hs/space here',
    `mxc://hs/${'é'.repeat(1024)}`,
  ])('rejects hostile image reference %s but preserves text', (value) => {
    expect(parse({ 'og:image': value })).toBeNull()
  })

  it.each([
    { 'og:image:width': 1e12 },
    { 'og:image:height': -1 },
    { 'og:image:width': '300' },
    { 'og:image:width': NaN },
    { 'og:image:width': 4000, 'og:image:height': 4000 },
    { 'matrix:image:size': 2 * 1024 * 1024 + 1 },
    { 'og:image:type': 'image/svg+xml' },
  ])('rejects unbounded/active encrypted metadata %j', (fields) => {
    expect(
      parse({ 'matrix:image:encrypted': encryptedPreviewFile, ...fields }),
    ).toBeNull()
  })

  it('allows large plain source images to use bounded server thumbnails', () => {
    expect(
      parse({
        'og:image': 'mxc://hs/hero',
        'og:image:width': 5000,
        'og:image:height': 3000,
        'matrix:image:size': 3 * 1024 * 1024,
      }),
    ).toEqual({ mxc: 'mxc://hs/hero', encrypted: false })
  })

  it.each([-1, NaN, '300'])(
    'rejects malformed dimensions for plain images: %s',
    (width) => {
      expect(
        parse({ 'og:image': 'mxc://hs/image', 'og:image:width': width }),
      ).toBeNull()
    },
  )

  it.each([
    null,
    {},
    { ...encryptedPreviewFile, v: 'v1' },
    { ...encryptedPreviewFile, key: {} },
    { ...encryptedPreviewFile, iv: 'bad' },
    { ...encryptedPreviewFile, hashes: {} },
    { ...encryptedPreviewFile, url: 'https://tracking.test/pixel' },
  ])(
    'does not downgrade invalid encrypted metadata to a plain alias',
    (file) => {
      expect(
        parse({
          'matrix:image:encrypted': file,
          'og:image': 'mxc://hs/plain',
          'beeper:image:encryption': encryptedPreviewFile,
        }),
      ).toBeNull()
    },
  )

  it('keeps matching-body, standard authority and spoiler rules for images', () => {
    const imageEntry = { ...entry, 'og:image': 'mxc://hs/image' }
    expect(parseLinkPreviews(bundle([imageEntry]), `${url}/other`)).toEqual([])
    expect(
      parseLinkPreviews({ ...bundle([imageEntry]), 'm.url_previews': [] }, url),
    ).toEqual([])
    expect(
      parseLinkPreviews(
        {
          ...bundle([imageEntry]),
          format: 'org.matrix.custom.html',
          formatted_body: `<span data-mx-spoiler>${url}</span>`,
        },
        url,
      ),
    ).toEqual([])
  })
})
