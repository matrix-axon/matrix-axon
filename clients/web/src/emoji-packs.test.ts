import { describe, expect, it } from 'vitest'
import { parseEmojiPacks } from './emoji-packs'

const image = (shortcode = 'wave', url = 'mxc://hs/wave') => ({
  shortcode,
  url,
})
const pack = (images: unknown[], room_id: string | null = null) => ({
  room_id,
  state_key: room_id === null ? null : 'Testing',
  event_type:
    room_id === null ? 'im.ponies.user_emotes' : 'im.ponies.room_emotes',
  images,
})

describe('emoji pack boundary', () => {
  it('keeps personal and room identities with duplicate names across packs', () => {
    const result = parseEmojiPacks({
      packs: [
        pack([image()]),
        pack([image('wave', 'mxc://hs/other')], '!room:hs'),
      ],
    })
    expect(result.partial).toBe(false)
    expect(result.packs.map((p) => p.images[0].url)).toEqual([
      'mxc://hs/wave',
      'mxc://hs/other',
    ])
    expect(result.packs[1].state_key).toBe('Testing')
  })
  it('rejects trackers, control names and normalized duplicates without losing usable siblings', () => {
    const result = parseEmojiPacks({
      packs: [
        pack([
          image(),
          image(' wave '),
          image('bad\nname'),
          image('tracker', 'https://tracker.test/x'),
        ]),
      ],
    })
    expect(result.partial).toBe(true)
    expect(result.packs[0].images).toEqual([{ ...image(), body: null }])
  })
  it('caps packs, per-pack examination and overall images', () => {
    const images = Array.from({ length: 600 }, (_, i) => image(`emoji${i}`))
    const result = parseEmojiPacks({
      packs: Array.from({ length: 70 }, () => pack(images)),
    })
    expect(result.partial).toBe(true)
    expect(result.packs.flatMap((p) => p.images)).toHaveLength(1000)
    expect(result.packs[0].images).toHaveLength(512)
    expect(result.packs[1].partial).toBe(true)
  })
  it('reports malformed envelopes and keeps valid empty packs', () => {
    expect(() => parseEmojiPacks(null)).toThrow()
    expect(parseEmojiPacks({ packs: [pack([])] }).partial).toBe(false)
  })
})
