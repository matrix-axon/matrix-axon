import { isApplePlatform, isPrimaryModifier } from './shortcuts'

/**
 * Markdown formatting shortcuts for the composer (ADR 0123).
 *
 * Everything here is pure: a keydown maps to a `FormatKind`, and a kind plus
 * the textarea's text and selection maps to a `FormatEdit`. The composer
 * applies the edit through the browser's own editing path so Ctrl-Z still
 * undoes it.
 */

export type FormatKind = 'bold' | 'italic' | 'strike' | 'code' | 'link'

/**
 * One replacement in the original text: `[from, to)` becomes `insert`, and the
 * selection afterwards is `[selectionStart, selectionEnd)` in the *new* text.
 * Keeping the edit minimal is what lets the caller feed it to `insertText`
 * and leave the rest of the undo history alone.
 */
export interface FormatEdit {
  from: number
  to: number
  insert: string
  selectionStart: number
  selectionEnd: number
}

/** The URL placeholder a new link leaves selected, ready to be typed over. */
export const LINK_URL_PLACEHOLDER = 'url'

/** The label a link with nothing selected starts with. */
const LINK_LABEL_PLACEHOLDER = 'link'

/**
 * The key each format answers to, with whether it needs Shift. The primary
 * modifier (⌘ on Apple, Ctrl elsewhere) is implied.
 *
 * The same chords serve the browser and the native shell. Bold, italic and
 * code take the platform's standard unshifted keys; strike and link use Shift
 * because the unshifted forms (`Ctrl-X` cut, `Ctrl-K` the app's room filter)
 * are taken. Chrome and Firefox let a page override `Ctrl-B`/`I`/`E`/`K`, so
 * no web-only fallback is needed; a chord a browser reserves outright (new
 * window, close tab) is not on this list.
 */
const FORMAT_KEYS: readonly {
  kind: FormatKind
  key: string
  shift: boolean
}[] = [
  { kind: 'bold', key: 'b', shift: false },
  { kind: 'italic', key: 'i', shift: false },
  { kind: 'code', key: 'e', shift: false },
  { kind: 'strike', key: 'x', shift: true },
  { kind: 'link', key: 'k', shift: true },
]

/**
 * Which format, if any, does this keydown ask for?
 *
 * Only the platform's primary modifier counts: on macOS `Ctrl-B`/`Ctrl-I` are
 * Emacs cursor keys that a text field honours, and on Windows AltGr arrives as
 * Ctrl+Alt and must keep typing characters. Matched on `event.key`, so the
 * chord follows the user's layout.
 */
export function formatKindForEvent(
  event: KeyboardEvent,
  apple = isApplePlatform(),
): FormatKind | null {
  if (event.altKey || !isPrimaryModifier(event, apple)) {
    return null
  }
  const key = event.key.toLowerCase()
  const match = FORMAT_KEYS.find(
    (entry) => entry.key === key && entry.shift === event.shiftKey,
  )
  return match?.kind ?? null
}

/** The Markdown marker each wrapping format puts on both sides. */
const MARKERS: Record<Exclude<FormatKind, 'link'>, string> = {
  bold: '**',
  italic: '*',
  strike: '~~',
  code: '`',
}

/** Length of the run of `ch` ending at `end` (backwards) or starting at `start`. */
function runBefore(text: string, end: number, ch: string): number {
  let n = 0
  while (end - n > 0 && text[end - n - 1] === ch) {
    n++
  }
  return n
}

function runAfter(text: string, start: number, ch: string): number {
  let n = 0
  while (start + n < text.length && text[start + n] === ch) {
    n++
  }
  return n
}

/**
 * Is the format already on, given `n` adjacent marker characters on each side?
 *
 * Bold and italic share `*`, so a run is a count, not a yes/no: `*` is italic,
 * `**` bold, `***` both. Italic is on for an odd run; bold for two or more.
 */
function isOn(kind: Exclude<FormatKind, 'link'>, n: number): boolean {
  const marker = MARKERS[kind]
  if (marker[0] !== '*') {
    return n >= marker.length
  }
  return kind === 'bold' ? n >= 2 : n % 2 === 1
}

function wrapEdit(
  kind: Exclude<FormatKind, 'link'>,
  text: string,
  start: number,
  end: number,
): FormatEdit {
  const marker = MARKERS[kind]
  const ch = marker[0]
  const selected = text.slice(start, end)

  // Markers just outside the selection: `**[x]**`.
  const outside = Math.min(runBefore(text, start, ch), runAfter(text, end, ch))
  if (isOn(kind, outside)) {
    return {
      from: start - marker.length,
      to: end + marker.length,
      insert: selected,
      selectionStart: start - marker.length,
      selectionEnd: end - marker.length,
    }
  }

  // Markers inside it: `[**x**]`. Bounded by the text itself so an all-`*`
  // selection is not counted from both ends.
  const inner = Math.min(
    runAfter(selected, 0, ch),
    runBefore(selected, selected.length, ch),
    Math.floor(selected.length / 2),
  )
  if (selected !== '' && isOn(kind, inner)) {
    const insert = selected.slice(marker.length, -marker.length)
    return {
      from: start,
      to: end,
      insert,
      selectionStart: start,
      selectionEnd: start + insert.length,
    }
  }

  return {
    from: start,
    to: end,
    insert: `${marker}${selected}${marker}`,
    selectionStart: start + marker.length,
    selectionEnd: end + marker.length,
  }
}

function linkEdit(text: string, start: number, end: number): FormatEdit {
  const label = text.slice(start, end) || LINK_LABEL_PLACEHOLDER
  const urlStart = start + label.length + 3
  return {
    from: start,
    to: end,
    insert: `[${label}](${LINK_URL_PLACEHOLDER})`,
    selectionStart: urlStart,
    selectionEnd: urlStart + LINK_URL_PLACEHOLDER.length,
  }
}

/**
 * The edit a format chord makes to `text` with `[start, end)` selected.
 *
 * Bold, italic, strike and code toggle: wrap the selection (or, with none,
 * insert an empty pair with the caret between), and unwrap it if it is already
 * wrapped. A link wraps the selection as its label and leaves the URL
 * placeholder selected.
 */
export function formatEdit(
  kind: FormatKind,
  text: string,
  start: number,
  end: number,
): FormatEdit {
  return kind === 'link'
    ? linkEdit(text, start, end)
    : wrapEdit(kind, text, start, end)
}

/**
 * Apply `edit` to a textarea the way typing would, so the browser records it
 * for Ctrl-Z. Assigning `value` (or `setRangeText`) discards the undo stack;
 * `execCommand` is deprecated but is still the only way to edit programmatically
 * and keep it. Where it is missing or refuses (jsdom), fall back to
 * `setRangeText` and raise the `input` event ourselves.
 */
export function applyFormatEdit(
  textarea: HTMLTextAreaElement,
  edit: FormatEdit,
): void {
  textarea.focus()
  textarea.setSelectionRange(edit.from, edit.to)
  const handled =
    typeof document.execCommand === 'function' &&
    (edit.insert === ''
      ? document.execCommand('delete')
      : document.execCommand('insertText', false, edit.insert))
  if (!handled) {
    textarea.setRangeText(edit.insert, edit.from, edit.to, 'end')
    textarea.dispatchEvent(new Event('input', { bubbles: true }))
  }
  textarea.setSelectionRange(edit.selectionStart, edit.selectionEnd)
}
