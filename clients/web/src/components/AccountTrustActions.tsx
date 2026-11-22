import { useServices } from '../services'
import type { Account } from '../stores/accounts'

export function AccountTrustActions({ account }: { account: Account }) {
  const { verification } = useServices()
  const query = new URLSearchParams({ recover: account.account_id })
  return (
    <span class="account-trust-actions">
      <button
        type="button"
        class="ghost"
        onClick={() =>
          verification.openPicker({
            accountId: account.account_id,
            ownDeviceId: account.device_id || null,
          })
        }
      >
        Verify this device
      </button>
      <a href={`/accounts?${query}`}>Recover keys</a>
    </span>
  )
}

export const OWN_IDENTITY_WARNING =
  'Your Matrix identity changed. Verify this Axon device again or use your recovery key.'
