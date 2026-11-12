import { describe, expect, it } from 'vitest'
import { lowercaseLinkSchemes, matchUrls, serverBodyUrls } from './linkify'
import { markdownCodeRanges } from '../markdown/markdown'

describe('matchUrls', () => {
  it.each([
    'matrix:r/a:bostoncoop.net',
    'matrix:room/a:bostoncoop.net',
    'matrix:r/ops%3Aexample.org?via=example.org',
  ])('matches bare Matrix room URI %s', (url) => {
    expect(matchUrls(`join ${url} now`)).toEqual([
      { url, start: 5, end: 5 + url.length },
    ])
  })
})

describe('capitalized http(s) schemes', () => {
  it.each([
    'Http://example.org/a',
    'Https://example.org/a',
    'HTTPS://example.org/a',
  ])('matches %s', (url) => {
    expect(matchUrls(`see ${url}.`)).toEqual([
      { url, start: 4, end: 4 + url.length },
    ])
  })
  it('keeps matrix: lowercase only, and skips a bare scheme in any case', () => {
    expect(matchUrls('Matrix:r/a:example.org Https:// HTTP://')).toEqual([])
  })
  it('lowercases only the scheme, leaving the length and everything else alone', () => {
    const text = 'Https://Example.org/A?Q=1 and HTTP://x.org/B'
    const lowered = lowercaseLinkSchemes(text)
    expect(lowered).toBe('https://Example.org/A?Q=1 and http://x.org/B')
    expect(lowered).toHaveLength(text.length)
    expect(lowercaseLinkSchemes('already https://a.org')).toBe(
      'already https://a.org',
    )
  })
  it('leaves links inside code as written', () => {
    const text = 'Https://a.org and `Https://b.org`\n```\nHttps://c.org\n```'
    expect(lowercaseLinkSchemes(text, markdownCodeRanges(text))).toBe(
      'https://a.org and `Https://b.org`\n```\nHttps://c.org\n```',
    )
  })
  it("mirrors the server's matcher, which wants a lowercase scheme", () => {
    expect([
      ...serverBodyUrls('Https://a.org https://b.org http://c.org'),
    ]).toEqual(['https://b.org', 'http://c.org'])
  })
})
