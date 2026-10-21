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
  // Mid-composition (an IME candidate string is open) the keydown belongs to
  // the IME; editing the text under it can corrupt the candidate. Some engines
  // report the composing keydown only as keyCode 229.
  if (event.isComposing || event.keyCode === 229) {
    return null
  }
  if (event.altKey || !isPrimaryModifier(event, apple)) {
    return null
  }
  const key = event.key.toLowerCase()
  const match = FORMAT_KEYS.find(
    (entry) => entry.key === key && entry.shift === event.shiftKey,
  )
  return match?.kind ?? null
}

/** The formats whose marker has a fixed length; code and link are special. */
type FixedKind = 'bold' | 'italic' | 'strike'

/** The Markdown marker each of them puts on both sides. */
const MARKERS: Record<FixedKind, string> = {
  bold: '**',
  italic: '*',
  strike: '~~',
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
function isOn(kind: FixedKind, n: number): boolean {
  const marker = MARKERS[kind]
  if (marker[0] !== '*') {
    return n >= marker.length
  }
  return kind === 'bold' ? n >= 2 : n % 2 === 1
}

function wrapEdit(
  kind: FixedKind,
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

/**
 * Inline code. A code span is delimited by a backtick run that no run inside it
 * matches, so the delimiter is sized to the content: `a`b` needs two. Content
 * that begins or ends with a backtick is padded with a space, which Markdown
 * strips back off, so the delimiter does not run into it.
 */
function codeEdit(text: string, start: number, end: number): FormatEdit {
  const selected = text.slice(start, end)

  // Delimiters just outside the selection. Take the whole run: it is the
  // delimiter, so a double-backtick span loses both ticks at once.
  const outside = Math.min(
    runBefore(text, start, '`'),
    runAfter(text, end, '`'),
  )
  if (outside > 0) {
    return {
      from: start - outside,
      to: end + outside,
      insert: selected,
      selectionStart: start - outside,
      selectionEnd: end - outside,
    }
  }
  // ... or the same with the padding space between them and the selection.
  const padBefore = /(`+) $/.exec(text.slice(0, start))
  const padAfter = /^ (`+)/.exec(text.slice(end))
  if (
    padBefore !== null &&
    padAfter !== null &&
    padBefore[1].length === padAfter[1].length
  ) {
    const cut = padBefore[1].length + 1
    return {
      from: start - cut,
      to: end + cut,
      insert: selected,
      selectionStart: start - cut,
      selectionEnd: end - cut,
    }
  }

  // Delimiters inside the selection.
  const inner = Math.min(
    runAfter(selected, 0, '`'),
    runBefore(selected, selected.length, '`'),
    Math.floor(selected.length / 2),
  )
  if (inner > 0) {
    let insert = selected.slice(inner, -inner)
    // Markdown strips one padding space from each side of a span that has
    // some other content.
    if (
      insert.length > 2 &&
      insert.startsWith(' ') &&
      insert.endsWith(' ') &&
      insert.trim() !== ''
    ) {
      insert = insert.slice(1, -1)
    }
    return {
      from: start,
      to: end,
      insert,
      selectionStart: start,
      selectionEnd: start + insert.length,
    }
  }

  const used = new Set((selected.match(/`+/g) ?? []).map((run) => run.length))
  let size = 1
  while (used.has(size)) {
    size++
  }
  const ticks = '`'.repeat(size)
  const pad = selected.startsWith('`') || selected.endsWith('`') ? ' ' : ''
  const open = ticks.length + pad.length
  return {
    from: start,
    to: end,
    insert: `${ticks}${pad}${selected}${pad}${ticks}`,
    selectionStart: start + open,
    selectionEnd: end + open,
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
  // Markers hug the text: a selection that takes in a space at either end
  // would otherwise give `**hello **`, which Markdown does not render as bold.
  // The whitespace stays outside, and a selection of nothing but whitespace is
  // treated as a caret at its end.
  const raw = text.slice(start, end)
  if (raw.trim() === '') {
    start = end
  } else {
    start += raw.length - raw.trimStart().length
    end -= raw.length - raw.trimEnd().length
  }
  if (kind === 'link') {
    return linkEdit(text, start, end)
  }
  return kind === 'code'
    ? codeEdit(text, start, end)
    : wrapEdit(kind, text, start, end)
}

/**
 * Apply `edit` to a textarea the way typing would, so the browser records it
 * for Ctrl-Z. Assigning `value` (or `setRangeText`) discards the undo stack;
 * `execCommand` is deprecated but is still the only way to edit programmatically
 * and keep it. Where it is missing or refuses (jsdom, or a browser that
 * returns false), fall back to `setRangeText` and raise the `input` event
 * ourselves. The fallback is a deliberate degradation: the edit still lands and
 * the draft updates, but it has no undo entry of its own.
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
