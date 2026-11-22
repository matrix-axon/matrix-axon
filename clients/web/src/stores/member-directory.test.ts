import { effect } from '@preact/signals'
import { describe, expect, it } from 'vitest'
import { createMemberDirectory, type MemberDirectory } from './member-directory'
import type { MemberDto } from './room-list'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const OTHER_ACCOUNT = '6b53f7f0-0000-4000-8000-000000000002'

function member(
  user_id: string,
  membership: string,
  display_name: string | null,
  avatar_url: string | null = null,
): MemberDto {
  return { user_id, membership, display_name, avatar_url } as MemberDto
}

/** Record a list asked for just now, under the account's current epoch. */
function learn(
  directory: MemberDirectory,
  accountId: string,
  members: readonly MemberDto[],
): void {
  directory.record(accountId, members, directory.epoch(accountId))
}

describe('createMemberDirectory', () => {
  it('lends a name learned in one room to any lookup on the same account', () => {
    const directory = createMemberDirectory()
    learn(directory, ACCOUNT, [member('@alice:hs', 'join', 'Alice')])

    expect(directory.lookup(ACCOUNT, '@alice:hs')?.display_name).toBe('Alice')
    // Accounts are separate readers: one's directory is not the other's.
    expect(directory.lookup(OTHER_ACCOUNT, '@alice:hs')).toBeUndefined()
  })

  it('never lets a departed membership overwrite a current one', () => {
    const directory = createMemberDirectory()
    learn(directory, ACCOUNT, [member('@alice:hs', 'join', 'Alice')])
    learn(directory, ACCOUNT, [member('@alice:hs', 'leave', 'Old Alice')])
    expect(directory.lookup(ACCOUNT, '@alice:hs')?.display_name).toBe('Alice')

    // A current one does replace it: the newest current name wins.
    learn(directory, ACCOUNT, [member('@alice:hs', 'join', 'Alice B.')])
    expect(directory.lookup(ACCOUNT, '@alice:hs')?.display_name).toBe(
      'Alice B.',
    )
  })

  it('skips members with nothing to lend', () => {
    const directory = createMemberDirectory()
    learn(directory, ACCOUNT, [
      member('@bare:hs', 'join', null),
      member('@blank:hs', 'join', '  '),
      member('@pic:hs', 'join', null, 'mxc://hs/pic'),
    ])
    expect(directory.size).toBe(1)
    expect(directory.lookup(ACCOUNT, '@pic:hs')?.avatar_url).toBe(
      'mxc://hs/pic',
    )
  })

  it('writes nothing for a list it already knows, so fallback renders stay put', () => {
    const directory = createMemberDirectory()
    let runs = 0
    const dispose = effect(() => {
      directory.lookup(ACCOUNT, '@alice:hs')
      runs += 1
    })
    learn(directory, ACCOUNT, [member('@alice:hs', 'join', 'Alice')])
    expect(runs).toBe(2)

    learn(directory, ACCOUNT, [member('@alice:hs', 'join', 'Alice')])
    directory.clear()
    directory.clear()
    // One write for the change, one for the first clear; the repeats wrote
    // nothing — which is also what keeps the sign-out effect from cycling.
    expect(runs).toBe(3)
    dispose()
  })

  it('keeps a known name or avatar when a later list leaves that field blank', () => {
    const directory = createMemberDirectory()
    learn(directory, ACCOUNT, [
      member('@alice:hs', 'join', 'Alice', 'mxc://hs/alice'),
    ])
    // The usual member event: a name, no avatar. Then a nameless one.
    learn(directory, ACCOUNT, [member('@alice:hs', 'join', 'Alice', null)])
    learn(directory, ACCOUNT, [
      member('@alice:hs', 'join', ' ', 'mxc://hs/alice2'),
    ])

    expect(directory.lookup(ACCOUNT, '@alice:hs')).toMatchObject({
      display_name: 'Alice',
      avatar_url: 'mxc://hs/alice2',
    })
  })

  it('drops a list asked for before a sign-out wiped the directory', () => {
    const directory = createMemberDirectory()
    const issued = directory.epoch(ACCOUNT)
    // Cleared while the list was in flight, even with nothing in it yet.
    directory.clear()
    directory.record(ACCOUNT, [member('@alice:hs', 'join', 'Alice')], issued)
    expect(directory.size).toBe(0)

    // A list asked for after the wipe is the new session's, and lands.
    learn(directory, ACCOUNT, [member('@alice:hs', 'join', 'Alice')])
    expect(directory.size).toBe(1)
  })

  it('forgets one account, and drops its lists still in flight', () => {
    const directory = createMemberDirectory()
    learn(directory, ACCOUNT, [member('@alice:hs', 'join', 'Alice')])
    learn(directory, OTHER_ACCOUNT, [member('@alice:hs', 'join', 'Alice O.')])
    const issued = directory.epoch(ACCOUNT)
    const otherIssued = directory.epoch(OTHER_ACCOUNT)

    directory.forgetAccount(ACCOUNT)
    directory.record(ACCOUNT, [member('@bob:hs', 'join', 'Bob')], issued)
    directory.record(
      OTHER_ACCOUNT,
      [member('@bob:hs', 'join', 'Bob O.')],
      otherIssued,
    )

    expect(directory.lookup(ACCOUNT, '@alice:hs')).toBeUndefined()
    expect(directory.lookup(ACCOUNT, '@bob:hs')).toBeUndefined()
    // The other account's names, and its in-flight list, are untouched.
    expect(directory.lookup(OTHER_ACCOUNT, '@alice:hs')?.display_name).toBe(
      'Alice O.',
    )
    expect(directory.lookup(OTHER_ACCOUNT, '@bob:hs')?.display_name).toBe(
      'Bob O.',
    )
  })

  it('keeps an epoch from a clear after a forget, and the other way round', () => {
    const directory = createMemberDirectory()
    const before = directory.epoch(ACCOUNT)
    directory.forgetAccount(ACCOUNT)
    const forgotten = directory.epoch(ACCOUNT)
    directory.clear()
    // Each wipe moves the epoch on; none moves it back to a ticket in hand.
    expect(forgotten).not.toBe(before)
    expect(directory.epoch(ACCOUNT)).not.toBe(forgotten)
    expect(directory.epoch(ACCOUNT)).not.toBe(before)
  })
})
