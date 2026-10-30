import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
import { previewPreflight } from './contract'
import type { components } from '../api/schema'
import { isIncomingShare } from './types'

const fixture = JSON.parse(
  readFileSync(
    '../../crates/axon-api/tests/fixtures/link-preview-contract.json',
    'utf8',
  ),
) as {
  cases: {
    name: string
    request: components['schemas']['SendMessageRequest']
    status: number
    code?: string
  }[]
}
describe('portable server preview contract', () => {
  it('projects the shared versioned manifest without serializing local thumbnail handles', () => {
    const record: unknown = JSON.parse(
      readFileSync('src/incoming-share/fixtures/manifest-v1.json', 'utf8'),
    )
    expect(isIncomingShare(record)).toBe(true)
    if (!isIncomingShare(record)) throw new Error('invalid portable manifest')
    expect(
      record.files.filter((file) => !file.thumbnail).map((file) => file.name),
    ).toEqual(['report.pdf', 'second.txt'])
    const wire = record.previews!.map(
      ({ matched_url, title, description }) => ({
        matched_url,
        title,
        description,
      }),
    )
    expect(
      previewPreflight({ body: record.text, url_previews: wire }, true),
    ).toBeNull()
    expect(JSON.stringify(wire)).not.toContain('thumbnail')
  })
  for (const entry of fixture.cases)
    it(entry.name, () => {
      const error = previewPreflight(entry.request)
      expect(error === null ? 200 : error === 'invalid' ? 400 : 413).toBe(
        entry.status,
      )
      if (entry.code !== undefined) expect(error).toBe(entry.code)
    })
  it('accepts empty optional fields and keeps the final event gate separate', () => {
    expect(
      previewPreflight({
        body: 'https://example.org',
        url_previews: [{ matched_url: 'https://example.org', title: '' }],
      }),
    ).toBeNull()
    expect(
      previewPreflight({ body: 'x'.repeat(41 * 1024), url_previews: [] }),
    ).toBe('event_too_large')
  })
})
