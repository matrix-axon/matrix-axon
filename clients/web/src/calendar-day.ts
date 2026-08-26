/** Local start-of-day for a calendar date, or null for an impossible one. */
function dayStart(year: number, month: number, day: number): number | null {
  const date = new Date(year, month - 1, day)
  // Date() rolls an impossible day into the next month; a roll means the
  // input was not a real date.
  if (
    date.getFullYear() !== year ||
    date.getMonth() !== month - 1 ||
    date.getDate() !== day
  ) {
    return null
  }
  return date.getTime()
}

export interface CalendarDayRange {
  start: number
  end: number
}

/**
 * One calendar day as an inclusive `[start, end]` ms range, local time.
 * Accepts ISO `YYYY-MM-DD`, US `MM/DD/YY[YY]` (two-digit years are 20xx,
 * matching the TUI), and the two relatives everyone actually types.
 */
export function parseCalendarDay(value: string): CalendarDayRange | null {
  const lower = value.trim().toLowerCase()
  let start: number | null = null
  if (lower === 'today' || lower === 'yesterday') {
    const now = new Date()
    // Calendar arithmetic, not `- DAY_MS`: around a DST transition the
    // previous local day is 23 or 25 hours long, and fixed-ms subtraction
    // from local midnight lands on the wrong day. Date() normalizes day 0
    // to the last day of the previous month.
    const dayOffset = lower === 'yesterday' ? 1 : 0
    const target = new Date(
      now.getFullYear(),
      now.getMonth(),
      now.getDate() - dayOffset,
    )
    start = dayStart(
      target.getFullYear(),
      target.getMonth() + 1,
      target.getDate(),
    )
  } else {
    const iso = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value.trim())
    const us = /^(\d{1,2})\/(\d{1,2})\/(\d{2}|\d{4})$/.exec(value.trim())
    if (iso !== null) {
      start = dayStart(Number(iso[1]), Number(iso[2]), Number(iso[3]))
    } else if (us !== null) {
      const year = Number(us[3])
      start = dayStart(
        year < 100 ? 2000 + year : year,
        Number(us[1]),
        Number(us[2]),
      )
    }
  }
  if (start === null) {
    return null
  }
  // End of the same calendar day, DST-safe: start of the next day minus 1ms.
  const startDate = new Date(start)
  const end =
    new Date(
      startDate.getFullYear(),
      startDate.getMonth(),
      startDate.getDate() + 1,
    ).getTime() - 1
  return { start, end }
}

/** `ts` -> the ISO date of the local calendar day containing it. */
export function isoCalendarDay(ts: number): string {
  const date = new Date(ts)
  const month = String(date.getMonth() + 1).padStart(2, '0')
  const day = String(date.getDate()).padStart(2, '0')
  return `${date.getFullYear()}-${month}-${day}`
}

export function sameLocalDay(a: number, b: number): boolean {
  return isoCalendarDay(a) === isoCalendarDay(b)
}

/** Timeline day heading: "Mon, Jun 1, 2026". */
export function formatTimelineDay(ts: number): string {
  return new Date(ts).toLocaleDateString(undefined, {
    weekday: 'short',
    year: 'numeric',
    month: 'short',
    day: 'numeric',
  })
}
