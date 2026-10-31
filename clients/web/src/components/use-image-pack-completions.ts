import { useEffect, useRef, useState } from 'preact/hooks'
import {
  loadImagePacks,
  imageShortcodeSuggestions,
  type EmojiPacks,
} from '../emoji-packs'
import type { StickerPickerOptions } from './StickerPicker'

const SNAPSHOT_TTL_MS = 30_000

/** Reuse at most two usage snapshots within this mounted account/room scope.
 * No global cache: unmount/sign-out drops the snapshot, and browsing/explicit
 * sends retain fresh discovery. Query prefixes always filter locally.
 */
export function useImagePackCompletions(
  options:
    Pick<StickerPickerOptions, 'api' | 'accountId' | 'roomId'> | undefined,
  query: string | null,
  usage: 'emoticon' | 'sticker',
) {
  const api = options?.api
  const accountId = options?.accountId
  const roomId = options?.roomId
  const active = api !== undefined && query !== null
  const id = `${accountId ?? ''}\0${roomId ?? ''}`
  const [snapshot, setSnapshot] = useState<{
    id: string
    api: StickerPickerOptions['api']
    usage: 'emoticon' | 'sticker'
    data: EmojiPacks
  } | null>(null)
  const cache = useRef<{
    id: string
    api: StickerPickerOptions['api']
    data: Map<'emoticon' | 'sticker', { loadedAt: number; data: EmojiPacks }>
  } | null>(null)
  useEffect(() => {
    if (!api || !accountId || !roomId) {
      cache.current = null
      setSnapshot(null)
      return
    }
    if (cache.current?.id !== id || cache.current.api !== api) {
      cache.current = { id, api, data: new Map() }
      setSnapshot(null)
    }
    if (!active) return
    const current = cache.current
    const cached = current.data.get(usage)
    if (cached && Date.now() - cached.loadedAt < SNAPSHOT_TTL_MS) {
      setSnapshot({ id, api, usage, data: cached.data })
      return
    }
    setSnapshot(null)
    const controller = new AbortController()
    let cancelled = false
    void loadImagePacks(api, accountId, roomId, usage, controller.signal)
      .then(({ data }) => {
        if (!cancelled && data) {
          current.data.set(usage, { loadedAt: Date.now(), data })
          setSnapshot({ id, api, usage, data })
        }
      })
      .catch(() => {}) // Unicode suggestions stay usable; Send reports discovery failures.
    return () => {
      cancelled = true
      controller.abort()
    }
  }, [api, accountId, roomId, active, id, usage])
  return active &&
    query !== null &&
    snapshot?.id === id &&
    snapshot.api === api &&
    snapshot.usage === usage
    ? imageShortcodeSuggestions(snapshot.data, query, usage)
    : []
}
