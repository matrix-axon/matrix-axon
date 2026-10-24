import { describe, expect, it } from 'vitest'
import { parseEmojiPacks, imageShortcodeSuggestions } from './emoji-packs'

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
    expect(result.packs[0].images).toEqual([
      { ...image(), body: null, emoticon: true, sticker: false, info: null },
    ])
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

it('filters independent sticker/emoticon usage without guessing stickers on old servers', () => {
  const value = {
    packs: [
      pack([
        image('legacy'),
        { ...image('emote'), emoticon: true, sticker: false },
        { ...image('sticker'), emoticon: false, sticker: true },
        { ...image('both'), emoticon: true, sticker: true },
      ]),
    ],
  }
  expect(
    parseEmojiPacks(value).packs[0].images.map((i) => i.shortcode),
  ).toEqual(['legacy', 'emote', 'both'])
  expect(
    parseEmojiPacks(value, 'sticker').packs[0].images.map((i) => i.shortcode),
  ).toEqual(['sticker', 'both'])
})

it('projects only bounded image metadata into sticker sends', () => {
  const metadata = {
    w: 320,
    h: 240,
    size: 42,
    mimetype: 'image/gif',
    is_animated: true,
  }
  const result = parseEmojiPacks(
    {
      packs: [
        pack([
          {
            ...image(),
            sticker: true,
            info: { ...metadata, arbitrary: '<script>' },
          },
          {
            ...image('bad'),
            sticker: true,
            info: {
              w: 16385,
              h: -1,
              size: Infinity,
              mimetype: 'text/html',
              is_animated: 'yes',
            },
          },
        ]),
      ],
    },
    'sticker',
  )
  expect(result.packs[0].images[0].info).toEqual(metadata)
  expect(result.packs[0].images[1].info).toEqual({})
})

it('bounds sticker completions and deduplicates case-folded names in source order', () => {
  const value = parseEmojiPacks(
    {
      packs: [
        pack([{ ...image('Adam'), sticker: true }]),
        pack(
          [
            { ...image('adam', 'mxc://hs/room-adam'), sticker: true },
            ...Array.from({ length: 30 }, (_, i) => ({
              ...image(`AD${i}`),
              sticker: true,
            })),
          ],
          '!room:hs',
        ),
      ],
    },
    'sticker',
  )
  const matches = imageShortcodeSuggestions(value, 'aD')
  expect(matches).toHaveLength(20)
  expect(matches[0].value).toBe(':Adam:')
  expect(
    matches.filter((m) => m.value.toLowerCase() === ':adam:'),
  ).toHaveLength(1)
  expect(matches[0].image.url).toBe('mxc://hs/wave')
})

it('keeps non-token legacy names out of shortcode completions', () => {
  const packs = parseEmojiPacks(
    {
      packs: [
        pack([
          { ...image('adam:other'), sticker: true },
          { ...image('adam'), sticker: true },
        ]),
      ],
    },
    'sticker',
  )
  expect(
    imageShortcodeSuggestions(packs, 'AD').map((match) => match.value),
  ).toEqual([':adam:'])
})
