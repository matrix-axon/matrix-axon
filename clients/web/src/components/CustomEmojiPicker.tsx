import { useEffect, useRef, useState } from 'preact/hooks'
import {
  loadImagePacks,
  type EmojiPacks,
  type CustomEmoji,
} from '../emoji-packs'
import { useServices } from '../services'
import { ReactionContent } from './ReactionContent'

type Load = { id: string; data?: EmojiPacks; error?: string }

/** A mount-scoped snapshot: never retain pack data across a room or session. */
export function CustomEmojiPicker({
  accountId,
  roomId,
  onReact,
}: {
  accountId: string
  roomId: string
  onReact: (key: string, shortcode: string) => void
}) {
  return (
    <ImagePackPicker
      accountId={accountId}
      roomId={roomId}
      onSelect={(image) => onReact(image.url, image.shortcode)}
    />
  )
}

export function ImagePackPicker({
  accountId,
  roomId,
  usage = 'emoticon',
  onSelect,
}: {
  accountId: string
  roomId: string
  usage?: 'emoticon' | 'sticker'
  onSelect: (image: CustomEmoji) => void
}) {
  const label = usage === 'sticker' ? 'stickers' : 'custom emoji'
  const searchLabel =
    usage === 'sticker' ? 'Search stickers' : 'Search custom emoji'
  const { api, live, auth } = useServices()
  const reconnects = live.reconnects.value
  const signedIn = auth.signedIn.value
  const [retry, setRetry] = useState(0)
  const [query, setQuery] = useState('')
  const [load, setLoad] = useState<Load | null>(null)
  const searchRef = useRef<HTMLInputElement>(null)
  const id = `${accountId}\0${roomId}\0${reconnects}\0${retry}\0${signedIn}\0${usage}`
  useEffect(() => {
    searchRef.current?.focus({ preventScroll: true })
  }, [])
  useEffect(() => {
    if (!signedIn) return
    const controller = new AbortController()
    let cancelled = false
    void (async () => {
      try {
        const { data, status } = await loadImagePacks(
          api,
          accountId,
          roomId,
          usage,
          controller.signal,
        )
        if (cancelled) return
        if (!data) {
          setLoad({
            id,
            error:
              status === 404
                ? `${usage === 'sticker' ? 'Stickers' : 'Custom emoji'} are unavailable for this room or server.`
                : `Could not load ${label}.`,
          })
          return
        }
        setLoad({ id, data })
      } catch {
        if (!cancelled) setLoad({ id, error: `Could not load ${label}.` })
      }
    })()
    return () => {
      cancelled = true
      controller.abort()
    }
  }, [api, accountId, roomId, signedIn, id, usage, label])

  const current = load?.id === id ? load : null
  const data = signedIn ? current?.data : undefined
  const search = query.trim().toLocaleLowerCase()
  const packs =
    data?.packs
      .map((pack) => ({
        ...pack,
        images: pack.images.filter((image) =>
          `${image.shortcode} ${image.body ?? ''} ${pack.display_name ?? ''}`
            .toLocaleLowerCase()
            .includes(search),
        ),
      }))
      .filter((pack) => pack.images.length > 0) ?? []
  return (
    <div class="custom-emoji-picker">
      <div class="custom-emoji-search">
        <input
          ref={searchRef}
          type="search"
          aria-label={searchLabel}
          placeholder={searchLabel}
          maxLength={100}
          value={query}
          onInput={(event) => setQuery(event.currentTarget.value)}
        />
        <button
          type="button"
          class="ghost"
          onClick={() => setRetry((value) => value + 1)}
          disabled={current === null}
        >
          Refresh
        </button>
      </div>
      <div
        class="custom-emoji-packs"
        aria-label={
          usage === 'sticker' ? 'Sticker packs' : 'Custom emoji packs'
        }
      >
        {current === null && <p role="status">Loading {label}…</p>}
        {current?.error && (
          <p role="status">
            {current.error}{' '}
            <button
              type="button"
              onClick={() => setRetry((value) => value + 1)}
            >
              Retry
            </button>
          </p>
        )}
        {data?.partial && <p role="status">Some {label} could not be shown.</p>}
        {data && packs.length === 0 && (
          <p role="status">
            {search
              ? `No matching ${label}.`
              : `No ${label} are available yet. Packs may still be syncing.`}
          </p>
        )}
        {packs.map((pack) => (
          <section
            key={`${pack.room_id ?? ''}\0${pack.event_type}\0${pack.state_key ?? ''}`}
          >
            <h3>
              {pack.display_name ||
                (pack.room_id === null
                  ? usage === 'sticker'
                    ? 'Personal stickers'
                    : 'Personal emoji'
                  : usage === 'sticker'
                    ? 'Room stickers'
                    : 'Room emoji')}
            </h3>
            <div class="custom-emoji-grid">
              {pack.images.map((image) => (
                <button
                  type="button"
                  key={image.shortcode}
                  title={`:${image.shortcode}:`}
                  aria-label={
                    usage === 'sticker'
                      ? `Send sticker :${image.shortcode}:`
                      : `React with :${image.shortcode}:`
                  }
                  onClick={() => onSelect(image)}
                >
                  <ReactionContent
                    accountId={accountId}
                    reaction={image.url}
                    shortcode={image.shortcode}
                  />
                  <span>{image.shortcode}</span>
                </button>
              ))}
            </div>
          </section>
        ))}
      </div>
    </div>
  )
}
