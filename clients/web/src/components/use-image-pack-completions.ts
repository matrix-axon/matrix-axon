import { useEffect, useState } from 'preact/hooks'
import {
  loadImagePacks,
  imageShortcodeSuggestions,
  type EmojiPacks,
} from '../emoji-packs'
import type { StickerPickerOptions } from './StickerPicker'

/** Fetch once per open query, then filter locally as its prefix changes. */
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
  useEffect(() => {
    setSnapshot(null)
    if (!active || !api || !accountId || !roomId) return
    const controller = new AbortController()
    let cancelled = false
    void loadImagePacks(api, accountId, roomId, usage, controller.signal)
      .then(({ data }) => {
        if (!cancelled && data) setSnapshot({ id, api, usage, data })
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
