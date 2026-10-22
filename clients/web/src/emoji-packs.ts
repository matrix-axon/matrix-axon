import type { components } from './api/schema'
import { customReactionMxc, reactionShortcode } from './custom-reactions'

export type EmojiPack = components['schemas']['EmojiPackDto']
export type CustomEmoji = components['schemas']['CustomEmojiDto']
export type EmojiPacks = components['schemas']['RoomEmojiPacksDto']

function object(value: unknown): Record<string, unknown> | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null
}

function text(value: unknown, max: number): string | null {
  return typeof value === 'string' &&
    value.length <= max &&
    !/\p{Cc}/u.test(value)
    ? value
    : null
}

/** Bound rendering even when a server returns malformed or excessive data. */
export function parseEmojiPacks(value: unknown): EmojiPacks {
  const root = object(value)
  if (!root || !Array.isArray(root.packs))
    throw new Error('Invalid emoji pack response')
  let partial = root.partial === true || root.packs.length > 65
  let remaining = 1000
  const packs: EmojiPack[] = []
  for (const raw of root.packs.slice(0, 65)) {
    if (remaining === 0) {
      partial = true
      break
    }
    const pack = object(raw)
    if (
      !pack ||
      !Array.isArray(pack.images) ||
      ![
        'm.room.image_pack',
        'im.ponies.room_emotes',
        'im.ponies.user_emotes',
      ].includes(String(pack.event_type)) ||
      !(pack.room_id === null || typeof pack.room_id === 'string') ||
      !(pack.state_key === null || typeof pack.state_key === 'string')
    ) {
      partial = true
      continue
    }
    let packPartial = pack.partial === true || pack.images.length > 512
    const images: CustomEmoji[] = []
    const names = new Set<string>()
    for (const rawImage of pack.images.slice(0, 512)) {
      if (remaining === 0) {
        packPartial = true
        break
      }
      const image = object(rawImage)
      const shortcode = reactionShortcode(image?.shortcode)
      const url =
        typeof image?.url === 'string' ? customReactionMxc(image.url) : null
      if (shortcode === null || url === null || names.has(shortcode)) {
        packPartial = true
        continue
      }
      names.add(shortcode)
      images.push({ shortcode, url, body: text(image?.body, 1024) })
      remaining--
    }
    partial ||= packPartial
    packs.push({
      room_id: pack.room_id as string | null,
      state_key: pack.state_key as string | null,
      event_type: pack.event_type as string,
      event_id: text(pack.event_id, 2048),
      display_name: text(pack.display_name, 256),
      avatar_url: null,
      images,
      partial: packPartial,
    })
  }
  return { packs, partial }
}
