import type { Notice } from './components/NoticeBanner'
import type { components } from './api/schema'

export type BackupAction = components['schemas']['BackupActionDto']
export type BackupSnapshot = components['schemas']['BackupSnapshotDto']

/**
 * TUI `lifecycle.rs` refuses `/backup enable` before the recovery key is sent.
 * Same copy here so an unverified web click does not put the key on the wire.
 */
export const UNVERIFIED_BACKUP_ENABLE_MESSAGE =
  'account is not verified; recover or verify first'

/**
 * Enable backup is still useful when this device is already uploading
 * (kick-upload, export-only resume). The web control stays clickable.
 */
export const BACKUP_ALREADY_UPLOADING_HINT =
  'Megolm backup is already enabled on this device. Click to retry upload or re-export into Matrix Server-Side Secret Storage (4S).'

export interface BackupBadge {
  label: string
  className: string
  title: string
}

/**
 * Honest `/status`-style lines for `AccountDto.backup` (ADR 0098).
 * Mirrors TUI `account_backup_status_lines` in `clients/tui/src/ui.rs`.
 * `recovery_state` is 4S completeness, not history-key import.
 */
export function backupSnapshotLines(backup: BackupSnapshot): {
  megolm: string
  secretStorage: string
} {
  const exists =
    backup.exists_on_server === true
      ? 'on homeserver'
      : backup.exists_on_server === false
        ? 'none on homeserver'
        : 'existence unknown'
  const uploading = backup.this_device_uploading
    ? 'this device uploading'
    : 'this device not uploading'
  return {
    megolm: `Megolm backup: ${exists}; ${uploading}; state ${backup.backup_state}`,
    secretStorage: `Matrix Server-Side Secret Storage (4S): ${backup.recovery_state}`,
  }
}

/**
 * Compact badge for the account card head. Unknown snapshots (deactivated,
 * probe skipped) skip the badge — the two-line snapshot still renders.
 * Titles explain the badge; they do not repeat the snapshot lines below.
 */
export function backupBadge(backup: BackupSnapshot): BackupBadge | null {
  if (backup.this_device_uploading) {
    return {
      label: 'backup',
      className: 'backup-uploading',
      title:
        'This Axon device is uploading megolm session keys to the homeserver backup. Keys it holds from now on can be recovered after a new login.',
    }
  }
  if (backup.exists_on_server === false) {
    return {
      label: 'no backup',
      className: 'backup-missing',
      title:
        'No megolm key backup on the homeserver. Matrix Server-Side Secret Storage (4S) being enabled does not mean history keys are backed up.',
    }
  }
  if (backup.exists_on_server === true) {
    return {
      label: 'backup elsewhere',
      className: 'backup-elsewhere',
      title:
        'A megolm backup exists on the homeserver, but this Axon device is not uploading to it. Recover or Enable backup to join.',
    }
  }
  return null
}

/**
 * TUI `backup_enable_success_status` without the user-id suffix — the web
 * card already names the account.
 */
export function backupActionMessage(action: BackupAction): string {
  switch (action) {
    case 'enabled':
      return 'Enabled megolm backup.'
    case 'joined':
      return 'Joined the existing megolm backup.'
    case 'already_uploading':
      return 'Already uploading megolm keys to backup.'
    case 'export_pending':
      return 'Megolm backup export pending — retry Enable backup.'
    case 'failed':
      return 'Megolm backup enable failed — retry Enable backup.'
  }
}

function backupActionTone(action: BackupAction): Notice['tone'] {
  return action === 'failed' || action === 'export_pending' ? 'info' : 'success'
}

export function enableBackupNotice(action: BackupAction): Notice {
  return {
    tone: backupActionTone(action),
    message: backupActionMessage(action),
  }
}

/**
 * Recover 200 is "4S import succeeded," not "history keys downloaded"
 * (ADR 0098 §5). Never say "Keys recovered."
 */
export function recoverHonestyNotice(
  verified: boolean | null | undefined,
  action?: BackupAction,
): Notice {
  const imported =
    verified === true
      ? 'Matrix Server-Side Secret Storage (4S) imported — this device is now verified.'
      : 'Matrix Server-Side Secret Storage (4S) imported. This device is still unverified — the recovery data may not include cross-signing keys.'
  if (action === undefined) {
    return {
      tone: verified === true ? 'success' : 'info',
      message: imported,
    }
  }
  const tone = verified === true ? backupActionTone(action) : 'info'
  return {
    tone,
    message: `${imported} ${backupActionMessage(action)}`,
  }
}
