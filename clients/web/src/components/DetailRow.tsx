import type { ComponentChildren } from 'preact'
import { CopyableText } from './CopyableText'
import type { Shown } from './room-details-format'

/**
 * One `<dt>`/`<dd>` pair in a detail list.
 *
 * `value` is real data, or `null`/empty when there is none; the row then shows
 * `empty` instead. Missing is never encoded as a magic string (issue 252): a
 * topic that is literally the word "None" is data, stays copyable, and is
 * never mistaken for the placeholder.
 */
export function DetailRow({
  label,
  value,
  empty = 'None',
  notes = [],
  code = false,
  copyable,
}: {
  label: string
  value: string | null | undefined
  /** Shown when `value` is `null` or empty; never copyable, never code. */
  empty?: string
  /** Muted qualifiers after the value or placeholder, e.g. "may be out of date". */
  notes?: readonly string[]
  code?: boolean
  /**
   * Whether a present `value` offers Copy; defaults to yes. User information
   * sets it to `false` for status text such as "Joined", which is a label
   * rather than data even though it is never empty.
   */
  copyable?: boolean
}) {
  const present = typeof value === 'string' && value !== ''
  const displayed: ComponentChildren = !present ? (
    empty
  ) : code ? (
    <code>{value}</code>
  ) : (
    value
  )
  return (
    <>
      <dt>{label}</dt>
      <dd>
        {present && (copyable ?? true) ? (
          <CopyableText text={value} label={label}>
            {displayed}
          </CopyableText>
        ) : (
          displayed
        )}
        {notes.length > 0 && (
          <span class="detail-note muted"> · {notes.join(' · ')}</span>
        )}
      </dd>
    </>
  )
}

/** `DetailRow` props for a formatted room detail (ADR 0133 decision 3). */
export function shownProps(shown: Shown): {
  value: string | null
  empty: string
  notes: readonly string[]
} {
  return shown.value !== null
    ? { value: shown.value, empty: 'None', notes: shown.notes }
    : { value: null, empty: shown.label, notes: shown.notes }
}
