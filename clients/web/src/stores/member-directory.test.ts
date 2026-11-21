import { effect } from '@preact/signals'
import { describe, expect, it } from 'vitest'
import { createMemberDirectory } from './member-directory'
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

describe('createMemberDirectory', () => {
  it('lends a name learned in one room to any lookup on the same account', () => {
    const directory = createMemberDirectory()
    directory.record(ACCOUNT, [member('@alice:hs', 'join', 'Alice')])

    expect(directory.lookup(ACCOUNT, '@alice:hs')?.display_name).toBe('Alice')
    // Accounts are separate readers: one's directory is not the other's.
    expect(directory.lookup(OTHER_ACCOUNT, '@alice:hs')).toBeUndefined()
  })

  it('never lets a departed membership overwrite a current one', () => {
    const directory = createMemberDirectory()
    directory.record(ACCOUNT, [member('@alice:hs', 'join', 'Alice')])
    directory.record(ACCOUNT, [member('@alice:hs', 'leave', 'Old Alice')])
    expect(directory.lookup(ACCOUNT, '@alice:hs')?.display_name).toBe('Alice')

    // A current one does replace it: the newest current name wins.
    directory.record(ACCOUNT, [member('@alice:hs', 'join', 'Alice B.')])
    expect(directory.lookup(ACCOUNT, '@alice:hs')?.display_name).toBe(
      'Alice B.',
    )
  })

  it('skips members with nothing to lend', () => {
    const directory = createMemberDirectory()
    directory.record(ACCOUNT, [
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
    directory.record(ACCOUNT, [member('@alice:hs', 'join', 'Alice')])
    expect(runs).toBe(2)

    directory.record(ACCOUNT, [member('@alice:hs', 'join', 'Alice')])
    directory.clear()
    directory.clear()
    // One write for the change, one for the first clear; the repeats wrote
    // nothing — which is also what keeps the sign-out effect from cycling.
    expect(runs).toBe(3)
    dispose()
  })
})
