import { describe, expect, it } from 'vitest'
import {
  BACKUP_ALREADY_UPLOADING_HINT,
  backupActionMessage,
  backupBadge,
  backupSnapshotLines,
  enableBackupNotice,
  recoverHonestyNotice,
  type BackupSnapshot,
} from './backup'

const UPLOADING: BackupSnapshot = {
  exists_on_server: true,
  this_device_uploading: true,
  backup_state: 'enabled',
  recovery_state: 'enabled',
}

const NONE_ON_SERVER: BackupSnapshot = {
  exists_on_server: false,
  this_device_uploading: false,
  backup_state: 'unknown',
  recovery_state: 'enabled',
}

const ELSEWHERE: BackupSnapshot = {
  exists_on_server: true,
  this_device_uploading: false,
  backup_state: 'enabled',
  recovery_state: 'enabled',
}

const UNKNOWN: BackupSnapshot = {
  exists_on_server: null,
  this_device_uploading: false,
  backup_state: 'unknown',
  recovery_state: 'unknown',
}

describe('backupSnapshotLines', () => {
  it('matches the TUI /status split of megolm backup vs 4S', () => {
    expect(backupSnapshotLines(UPLOADING)).toEqual({
      megolm:
        'Megolm backup: on homeserver; this device uploading; state enabled',
      secretStorage: 'Matrix Server-Side Secret Storage (4S): enabled',
    })
    expect(backupSnapshotLines(NONE_ON_SERVER).megolm).toContain(
      'none on homeserver',
    )
    expect(backupSnapshotLines(UNKNOWN).megolm).toContain('existence unknown')
    expect(backupSnapshotLines(UNKNOWN).secretStorage).toBe(
      'Matrix Server-Side Secret Storage (4S): unknown',
    )
  })
})

describe('backupBadge', () => {
  it('badges uploading, missing, and elsewhere; skips unknown', () => {
    expect(backupBadge(UPLOADING)?.label).toBe('backup')
    expect(backupBadge(UPLOADING)?.title).toMatch(
      /uploading megolm session keys/i,
    )
    expect(backupBadge(NONE_ON_SERVER)?.label).toBe('no backup')
    expect(backupBadge(NONE_ON_SERVER)?.title).toMatch(
      /does not mean history keys are backed up/i,
    )
    expect(backupBadge(ELSEWHERE)?.label).toBe('backup elsewhere')
    expect(backupBadge(ELSEWHERE)?.title).toMatch(/not uploading/i)
    expect(backupBadge(UNKNOWN)).toBeNull()
  })

  it('explains that Enable backup is still a retry when already uploading', () => {
    expect(BACKUP_ALREADY_UPLOADING_HINT).toMatch(/already enabled/i)
    expect(BACKUP_ALREADY_UPLOADING_HINT).toMatch(/click to retry/i)
  })
})

describe('backupActionMessage', () => {
  it('covers every closed enum member', () => {
    expect(backupActionMessage('enabled')).toMatch(/enabled megolm backup/i)
    expect(backupActionMessage('joined')).toMatch(/joined the existing/i)
    expect(backupActionMessage('already_uploading')).toMatch(
      /already uploading/i,
    )
    expect(backupActionMessage('export_pending')).toMatch(/export pending/i)
    expect(backupActionMessage('failed')).toMatch(/enable failed/i)
  })
})

describe('recoverHonestyNotice', () => {
  it('never says keys recovered', () => {
    const verified = recoverHonestyNotice(true, 'enabled')
    expect(verified.message).not.toMatch(/keys recovered/i)
    expect(verified.message).toMatch(
      /server-side secret storage \(4s\) imported/i,
    )
    expect(verified.message).toMatch(/now verified/i)
    expect(verified.message).toMatch(/enabled megolm backup/i)
    expect(verified.tone).toBe('success')

    const unverified = recoverHonestyNotice(false, 'failed')
    expect(unverified.message).toMatch(/still unverified/i)
    expect(unverified.message).toMatch(/enable failed/i)
    expect(unverified.tone).toBe('info')
  })

  it('keeps the 4S sentence when backup_action is absent', () => {
    expect(recoverHonestyNotice(true).message).toBe(
      'Matrix Server-Side Secret Storage (4S) imported — this device is now verified.',
    )
    expect(recoverHonestyNotice(true).message).not.toMatch(/keys recovered/i)
  })
})

describe('enableBackupNotice', () => {
  it('uses info tone for failed and export-pending', () => {
    expect(enableBackupNotice('enabled').tone).toBe('success')
    expect(enableBackupNotice('failed').tone).toBe('info')
    expect(enableBackupNotice('export_pending').tone).toBe('info')
  })
})
