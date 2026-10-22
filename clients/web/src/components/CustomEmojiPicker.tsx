import { useEffect, useRef, useState } from 'preact/hooks'
import { parseEmojiPacks, type EmojiPacks } from '../emoji-packs'
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
  const { api, live, auth } = useServices()
  const reconnects = live.reconnects.value
  const signedIn = auth.signedIn.value
  const [retry, setRetry] = useState(0)
  const [query, setQuery] = useState('')
  const [load, setLoad] = useState<Load | null>(null)
  const searchRef = useRef<HTMLInputElement>(null)
  const id = `${accountId}\0${roomId}\0${reconnects}\0${retry}\0${signedIn}`
  useEffect(() => {
    searchRef.current?.focus({ preventScroll: true })
  }, [])
  useEffect(() => {
    if (!signedIn) return
    const controller = new AbortController()
    let cancelled = false
    void (async () => {
      try {
        const { data, response } = await api.GET(
          '/v1/accounts/{account_id}/rooms/{room_id}/emoji-packs',
          {
            params: { path: { account_id: accountId, room_id: roomId } },
            signal: controller.signal,
          },
        )
        if (cancelled) return
        if (!data) {
          setLoad({
            id,
            error:
              response.status === 404
                ? 'Custom emoji are unavailable for this room or server.'
                : 'Could not load custom emoji.',
          })
          return
        }
        setLoad({ id, data: parseEmojiPacks(data.data) })
      } catch {
        if (!cancelled) setLoad({ id, error: 'Could not load custom emoji.' })
      }
    })()
    return () => {
      cancelled = true
      controller.abort()
    }
  }, [api, accountId, roomId, signedIn, id])

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
          aria-label="Search custom emoji"
          placeholder="Search custom emoji"
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
      <div class="custom-emoji-packs" aria-label="Custom emoji packs">
        {current === null && <p role="status">Loading custom emoji…</p>}
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
        {data?.partial && (
          <p role="status">Some custom emoji could not be shown.</p>
        )}
        {data && packs.length === 0 && (
          <p role="status">
            {search
              ? 'No matching custom emoji.'
              : 'No custom emoji are available yet. Packs may still be syncing.'}
          </p>
        )}
        {packs.map((pack) => (
          <section
            key={`${pack.room_id ?? ''}\0${pack.event_type}\0${pack.state_key ?? ''}`}
          >
            <h3>
              {pack.display_name ||
                (pack.room_id === null ? 'Personal emoji' : 'Room emoji')}
            </h3>
            <div class="custom-emoji-grid">
              {pack.images.map((image) => (
                <button
                  type="button"
                  key={image.shortcode}
                  title={`:${image.shortcode}:`}
                  aria-label={`React with :${image.shortcode}:`}
                  onClick={() => onReact(image.url, image.shortcode)}
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
