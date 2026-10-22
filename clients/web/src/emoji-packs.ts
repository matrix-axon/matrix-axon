import type { ApiClient } from './api/client'
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
export function parseEmojiPacks(
  value: unknown,
  usage: 'emoticon' | 'sticker' = 'emoticon',
): EmojiPacks {
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
      const emoticon = image?.emoticon !== false
      const sticker = image?.sticker === true
      if (usage === 'emoticon' ? !emoticon : !sticker) continue
      images.push({
        shortcode,
        url,
        body: text(image?.body, 1024),
        emoticon,
        sticker,
        info: parseStickerInfo(image?.info),
      })
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

/** Only bounded known ImageInfo fields enter a send. */
function parseStickerInfo(value: unknown): CustomEmoji['info'] {
  const raw = object(value)
  if (!raw) return null
  const info: NonNullable<CustomEmoji['info']> = {}
  for (const key of ['w', 'h', 'size'] as const) {
    const limit = key === 'size' ? 104857600 : 16384
    const n = raw[key]
    if (
      typeof n === 'number' &&
      Number.isSafeInteger(n) &&
      n <= limit &&
      n >= (key === 'size' ? 0 : 1)
    )
      info[key] = n
  }
  if (
    typeof raw.mimetype === 'string' &&
    ['image/png', 'image/jpeg', 'image/gif', 'image/webp'].includes(
      raw.mimetype,
    )
  )
    info.mimetype = raw.mimetype
  if (typeof raw.is_animated === 'boolean') info.is_animated = raw.is_animated
  return info
}

/** Keep sticker names and metadata consistent across room and thread sends. */
export function stickerFromImage(image: CustomEmoji) {
  return {
    url: image.url,
    body: image.body?.trim() ? image.body : image.shortcode,
    info: image.info,
  }
}

/** Shared discovery for picker browsing and explicit shortcode sends. */
export async function loadImagePacks(
  api: ApiClient,
  accountId: string,
  roomId: string,
  usage: 'emoticon' | 'sticker',
  signal: AbortSignal,
) {
  const { data, response } = await api.GET(
    '/v1/accounts/{account_id}/rooms/{room_id}/emoji-packs',
    {
      params: {
        path: { account_id: accountId, room_id: roomId },
        query: usage === 'sticker' ? { usage: 'sticker' } : undefined,
      },
      signal,
    },
  )
  return {
    data: data ? parseEmojiPacks(data.data, usage) : undefined,
    status: response.status,
  }
}

/** Only a complete, standalone token can become a standalone Matrix sticker. */
export function standaloneStickerShortcode(body: string): string | null {
  const value = body.trim()
  if (value.length > 1026) return null
  return /^:([^:\r\n]+):$/u.exec(value)?.[1] ?? null
}

export async function lookupStickerShortcode(
  api: ApiClient,
  accountId: string,
  roomId: string,
  shortcode: string,
  signal: AbortSignal,
): Promise<CustomEmoji | null> {
  const { data, status } = await loadImagePacks(
    api,
    accountId,
    roomId,
    'sticker',
    signal,
  )
  // An older server has no discovery route; ordinary shortcode handling stays usable.
  if (!data) {
    if (status === 404) return null
    throw new Error('Sticker discovery failed')
  }
  // Server order is personal first, then deterministic room-pack order.
  for (const pack of data.packs) {
    const image = pack.images.find((image) => image.shortcode === shortcode)
    if (image) return image
  }
  return null
}
