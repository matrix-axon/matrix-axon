/**
 * The last segment of a path, for either separator, or `fallback` when the
 * path has no usable last segment.
 *
 * One function for the two places a path has to be reduced to a name, which
 * make opposite mistakes if they drift. The save dialog (`platform/tauri.ts`)
 * is handed `content.filename` or `content.body` off a room event, which is
 * to say a string whoever sent the media chose — `../../.config/autostart/
 * evil.desktop` is a filename as far as the room is concerned — and it must
 * never reach the dialog as a `defaultPath` with a directory in it. The native
 * drop path (`media/dropped-file.ts`) is handed a real OS path and must not
 * show `C:\Users\…` to the user or send it to the room as the filename.
 *
 * Both separators are checked on every platform rather than branching on the
 * host: a Windows name can reach a Linux client through a room, a shared
 * volume or a test, and the reverse.
 *
 * `''`, `.` and `..` are not names. A save dialog handed `''` shows nothing
 * to save as, and a `File` called `..` is a filename nobody typed either, so
 * both fall back instead.
 */
export function basename(path: string, fallback = 'download'): string {
  const last = path.split(/[/\\]/).pop() ?? ''
  const trimmed = last.trim()
  return trimmed === '' || trimmed === '.' || trimmed === '..'
    ? fallback
    : trimmed
}
