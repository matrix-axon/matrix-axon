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
}: {
  label: string
  value: string | null | undefined
  code?: boolean
}) {
  const text = value ?? ''
  const displayed: ComponentChildren = code ? <code>{text}</code> : text
  return (
    <>
      <dt>{label}</dt>
      <dd>
        {isPopulatedDetail(value) ? (
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
