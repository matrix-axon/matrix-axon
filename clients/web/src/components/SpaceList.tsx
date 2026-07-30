import { useMemo, useRef } from 'preact/hooks'
import { useMediaBlob } from '../media/use-media-blob'
import { useServices } from '../services'
import { roomKey, roomTitle, type RoomDto } from '../stores/room-list'

/** The space picker scopes the neighboring RoomList; it never changes Matrix state. */
export function SpaceList() {
  const { rooms, spaces, settings } = useServices()
  const dragging = useRef<string | null>(null)
  const selected = spaces.selected.value
  const entries = useMemo(
    () =>
      orderSpaces(
        rooms.rooms.value.filter((room) => room.room_type === 'm.space'),
        settings.spaceOrder.value,
        rooms.titles.value,
      ),
    [rooms.rooms.value, settings.spaceOrder.value, rooms.titles.value],
  )

  return (
    <section id="spaces-pane" class="space-picker" aria-label="Spaces">
      <h2>Spaces</h2>
      <button
        type="button"
        class={
          selected === null ? 'space-picker-entry active' : 'space-picker-entry'
        }
        aria-pressed={selected === null}
        onClick={() => (spaces.selected.value = null)}
        title="All rooms"
        aria-label="All rooms"
      >
        All
      </button>
      {entries.map((space, index) => {
        const key = roomKey(space)
        const title = roomTitle(space, rooms.titles.value)
        return (
          <div
            class="space-picker-row"
            key={key}
            draggable
            onDragStart={() => {
              dragging.current = key
            }}
            onDragOver={(event) => event.preventDefault()}
            onDrop={(event) => {
              event.preventDefault()
              const source = dragging.current
              dragging.current = null
              if (source !== null) settings.moveSpace(source, index)
            }}
          >
            <button
              type="button"
              class={
                selected === key
                  ? 'space-picker-entry active'
                  : 'space-picker-entry'
              }
              aria-pressed={selected === key}
              onClick={() => (spaces.selected.value = key)}
              title={title}
              aria-label={title}
            >
              <SpaceAvatar
                accountId={space.account_id}
                room={space}
                title={title}
              />
            </button>
          </div>
        )
      })}
    </section>
  )
}

function SpaceAvatar({
  accountId,
  room,
  title,
}: {
  accountId: string
  room: RoomDto
  title: string
}) {
  const { ref, state } = useMediaBlob<HTMLSpanElement>(
    accountId,
    room.avatar_url ?? null,
  )
  const initial = title.trim().slice(0, 1).toLocaleUpperCase() || '?'
  return (
    <span ref={ref} class="space-avatar" aria-hidden="true">
      {state.status === 'ready' && state.url !== undefined ? (
        <img src={state.url} alt="" />
      ) : (
        initial
      )}
    </span>
  )
}

function orderSpaces(
  spaces: readonly RoomDto[],
  order: readonly string[],
  titles: ReadonlyMap<string, string>,
): RoomDto[] {
  const rank = new Map(order.map((key, index) => [key, index]))
  return [...spaces].sort((left, right) => {
    const leftRank = rank.get(roomKey(left))
    const rightRank = rank.get(roomKey(right))
    if (leftRank !== undefined || rightRank !== undefined) {
      if (leftRank === undefined) return 1
      if (rightRank === undefined) return -1
      return leftRank - rightRank
    }
    return roomTitle(left, titles).localeCompare(roomTitle(right, titles))
  })
}
