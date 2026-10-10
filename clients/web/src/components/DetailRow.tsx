import type { ComponentChildren } from 'preact'
import { CopyableText } from './CopyableText'

/** Placeholders are not worth copying; real IDs, names, and state are. */
function isPopulatedDetail(value: string | null | undefined): value is string {
  return (
    typeof value === 'string' &&
    value !== '' &&
    value !== 'None' &&
    value !== 'Unavailable' &&
    value !== 'Loading…' &&
    !value.startsWith('Unavailable from')
  )
}

export function DetailRow({
  label,
  value,
  code = false,
  copyable,
}: {
  label: string
  value: string | null | undefined
  code?: boolean
  /**
   * Overrides the placeholder heuristic. Room information leaves this unset.
   * User information sets it so a display name of "None" stays copyable and
   * status text such as "Joined" does not.
   */
  copyable?: boolean
}) {
  const text = value ?? ''
  const displayed: ComponentChildren = code ? <code>{text}</code> : text
  const canCopy = copyable ?? isPopulatedDetail(value)
  return (
    <>
      <dt>{label}</dt>
      <dd>
        {canCopy && typeof value === 'string' && value !== '' ? (
          <CopyableText text={value} label={label}>
            {displayed}
          </CopyableText>
        ) : (
          displayed
        )}
      </dd>
    </>
  )
}
