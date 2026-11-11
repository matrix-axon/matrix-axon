/**
 * A compact age: "now" under a minute, then "12m", "3h" or "5d", and the
 * calendar date (YYYY-MM-DD) from a week on. Shared by the room list's
 * activity column and Room Information's "updated …" notes, so the two read
 * the same way.
 */
export function relativeTime(ts: number, now: number = Date.now()): string {
  const seconds = Math.round((now - ts) / 1000)
  if (seconds < 60) {
    return 'now'
  }
  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}m`
  }
  if (seconds < 86400) {
    return `${Math.floor(seconds / 3600)}h`
  }
  if (seconds < 7 * 86400) {
    return `${Math.floor(seconds / 86400)}d`
  }
  return new Date(ts).toISOString().slice(0, 10)
}
