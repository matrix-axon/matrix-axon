import { formatTimelineDay } from '../calendar-day'

/** Day heading used by the room timeline and thread timelines. */
export function DaySeparator({ ts }: { ts: number }) {
  return (
    <li class="day-separator" role="separator">
      {formatTimelineDay(ts)}
    </li>
  )
}
