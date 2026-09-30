import { render } from '@testing-library/preact'
import { describe, expect, it } from 'vitest'
import { PrivacyPage } from './PrivacyPage'

describe('PrivacyPage', () => {
  it('renders the policy bundled from docs/PRIVACY_POLICY.md', () => {
    const { getByRole } = render(<PrivacyPage />)

    expect(getByRole('heading', { name: 'Axon Privacy Policy' })).toBeTruthy()
    expect(getByRole('heading', { name: 'Data we collect' })).toBeTruthy()
    expect(getByRole('link', { name: '← Back to settings' })).toBeTruthy()
  })

  it('sends the contact link out of the app', () => {
    const { getByRole } = render(<PrivacyPage />)
    const issues = getByRole('link', {
      name: 'https://github.com/matrix-axon/matrix-axon/issues',
    })

    expect(issues.getAttribute('target')).toBe('_blank')
    expect(issues.getAttribute('rel')).toBe('noopener noreferrer')
  })
})
