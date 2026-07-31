import { describe, expect, it } from 'vitest'
import { memoryStorage } from '../test/memory-storage'
import { applyTheme, createSettingsStore } from './settings'

describe('createSettingsStore', () => {
  it('starts from defaults with empty storage and persists them', () => {
    const storage = memoryStorage()
    const store = createSettingsStore(storage)
    expect(store.theme.value).toBe('system')
    expect(store.activeAccountId.value).toBeNull()
    expect(JSON.parse(storage.getItem('axon.settings')!)).toEqual({
      version: 1,
      theme: 'system',
      timeFormat: '12h',
      activeAccountId: null,
      pinnedRooms: [],
      spaceOrder: [],
      spacesPaneCollapsed: false,
      spacesPaneAutoHide: true,
      sidebarWidth: 420,
      roomSort: 'recent',
      roomFilter: 'all',
      sidebarCollapsed: false,
      stateEvents: 'important',
      hideRedactedEvents: false,
      previewRoom: true,
      messageComposerHeight: null,
      matrixProtocolHandler: false,
      recentReactions: [],
      developerMode: false,
      perfMarks: false,
      appBadgeEnabled: true,
      cacheRoomList: true,
    })
  })

  it('round-trips changes through storage', () => {
    const storage = memoryStorage()
    const first = createSettingsStore(storage)
    first.theme.value = 'dark'
    first.activeAccountId.value = 'acct-1'

    const second = createSettingsStore(storage)
    expect(second.theme.value).toBe('dark')
    expect(second.activeAccountId.value).toBe('acct-1')
  })

  it.each([
    ['corrupt JSON', 'not json{'],
    ['wrong version', JSON.stringify({ version: 99, theme: 'dark' })],
    ['non-object', JSON.stringify('dark')],
    ['bad theme value', JSON.stringify({ version: 1, theme: 'neon' })],
  ])('resets to defaults on %s', (_label, raw) => {
    const store = createSettingsStore(memoryStorage({ 'axon.settings': raw }))
    expect(store.theme.value).toBe('system')
    expect(store.activeAccountId.value).toBeNull()
  })

  it('keeps a valid stored envelope', () => {
    const store = createSettingsStore(
      memoryStorage({
        'axon.settings': JSON.stringify({
          version: 1,
          theme: 'light',
          activeAccountId: 'acct-9',
        }),
      }),
    )
    expect(store.theme.value).toBe('light')
    expect(store.activeAccountId.value).toBe('acct-9')
  })
})

describe('room-list settings (ADRs 0038/0042)', () => {
  it('an M-W3-era envelope without the new fields parses with defaults', () => {
    const store = createSettingsStore(
      memoryStorage({
        'axon.settings': JSON.stringify({
          version: 1,
          theme: 'dark',
          activeAccountId: null,
        }),
      }),
    )
    expect(store.pinnedRooms.value).toEqual([])
    expect(store.spaceOrder.value).toEqual([])
    expect(store.spacesPaneCollapsed.value).toBe(false)
    expect(store.spacesPaneAutoHide.value).toBe(true)
    expect(store.sidebarWidth.value).toBe(420)
    expect(store.roomSort.value).toBe('recent')
    expect(store.roomFilter.value).toBe('all')
    expect(store.theme.value).toBe('dark')
  })

  it('persists browser-local space ordering', () => {
    const storage = memoryStorage()
    const store = createSettingsStore(storage)
    const shown = ['account/!one:hs', 'account/!two:hs']
    store.moveSpace('account/!one:hs', 0, shown)
    store.moveSpace('account/!two:hs', 1, shown)
    store.moveSpace('account/!two:hs', 0, shown)
    store.spacesPaneCollapsed.value = true
    store.spacesPaneAutoHide.value = false
    store.sidebarWidth.value = 480
    expect(store.spaceOrder.value).toEqual([
      'account/!two:hs',
      'account/!one:hs',
    ])
    expect(createSettingsStore(storage).spaceOrder.value).toEqual([
      'account/!two:hs',
      'account/!one:hs',
    ])
    expect(createSettingsStore(storage).spacesPaneCollapsed.value).toBe(true)
    expect(createSettingsStore(storage).spacesPaneAutoHide.value).toBe(false)
    expect(createSettingsStore(storage).sidebarWidth.value).toBe(480)
  })

  it('moves a space down the displayed order, not the stored one', () => {
    const store = createSettingsStore(memoryStorage())
    const shown = ['account/!one:hs', 'account/!two:hs', 'account/!three:hs']
    // Nothing is ranked yet, so the stored order is empty: the new position has
    // to be resolved against what the picker actually shows.
    store.moveSpace('account/!one:hs', 1, shown)
    expect(store.spaceOrder.value).toEqual([
      'account/!two:hs',
      'account/!one:hs',
      'account/!three:hs',
    ])
    store.moveSpace('account/!three:hs', 0, store.spaceOrder.value)
    expect(store.spaceOrder.value).toEqual([
      'account/!three:hs',
      'account/!two:hs',
      'account/!one:hs',
    ])
  })

  it('keeps ranks for spaces the picker is not showing', () => {
    const storage = memoryStorage({
      'axon.settings': JSON.stringify({
        version: 1,
        spaceOrder: ['account/!gone:hs', 'account/!one:hs'],
      }),
    })
    const store = createSettingsStore(storage)
    store.moveSpace('account/!two:hs', 0, [
      'account/!one:hs',
      'account/!two:hs',
    ])
    expect(store.spaceOrder.value).toContain('account/!gone:hs')
    expect(store.spaceOrder.value.slice(0, 2)).toEqual([
      'account/!two:hs',
      'account/!one:hs',
    ])
  })

  it('an envelope without sidebarCollapsed defaults it to false (ADR 0062)', () => {
    const storage = memoryStorage({
      'axon.settings': JSON.stringify({
        version: 1,
        theme: 'dark',
        activeAccountId: null,
        roomSort: 'az',
      }),
    })
    const store = createSettingsStore(storage)
    expect(store.sidebarCollapsed.value).toBe(false)
    // A non-boolean is rejected rather than coerced.
    expect(
      createSettingsStore(
        memoryStorage({
          'axon.settings': JSON.stringify({
            version: 1,
            sidebarCollapsed: 'yes',
          }),
        }),
      ).sidebarCollapsed.value,
    ).toBe(false)

    store.sidebarCollapsed.value = true
    expect(createSettingsStore(storage).sidebarCollapsed.value).toBe(true)
  })

  it('stateEvents defaults to important and round-trips', () => {
    const storage = memoryStorage({
      'axon.settings': JSON.stringify({ version: 1, theme: 'dark' }),
    })
    const store = createSettingsStore(storage)
    expect(store.stateEvents.value).toBe('important')

    store.stateEvents.value = 'all'
    expect(createSettingsStore(storage).stateEvents.value).toBe('all')

    // An unknown value is rejected rather than coerced.
    expect(
      createSettingsStore(
        memoryStorage({
          'axon.settings': JSON.stringify({ version: 1, stateEvents: 'some' }),
        }),
      ).stateEvents.value,
    ).toBe('important')
  })

  it('migrates the pre-0079 showStateEvents boolean, keeping other settings', () => {
    const storage = memoryStorage({
      'axon.settings': JSON.stringify({
        version: 1,
        theme: 'dark',
        showStateEvents: true,
      }),
    })
    const store = createSettingsStore(storage)
    expect(store.stateEvents.value).toBe('all')
    // The migration must not cost the user their unrelated settings.
    expect(store.theme.value).toBe('dark')
    // …and the legacy key is not written back.
    expect(
      JSON.parse(storage.getItem('axon.settings')!).showStateEvents,
    ).toBeUndefined()

    expect(
      createSettingsStore(
        memoryStorage({
          'axon.settings': JSON.stringify({
            version: 1,
            showStateEvents: false,
          }),
        }),
      ).stateEvents.value,
    ).toBe('important')
  })

  it('hideRedactedEvents defaults to off and round-trips', () => {
    const storage = memoryStorage({
      'axon.settings': JSON.stringify({ version: 1, theme: 'dark' }),
    })
    const store = createSettingsStore(storage)
    expect(store.hideRedactedEvents.value).toBe(false)

    store.hideRedactedEvents.value = true
    expect(createSettingsStore(storage).hideRedactedEvents.value).toBe(true)

    // A non-boolean is rejected rather than coerced.
    expect(
      createSettingsStore(
        memoryStorage({
          'axon.settings': JSON.stringify({
            version: 1,
            hideRedactedEvents: 'yes',
          }),
        }),
      ).hideRedactedEvents.value,
    ).toBe(false)
  })

  it('previewRoom defaults to on and round-trips explicit off', () => {
    const storage = memoryStorage({
      'axon.settings': JSON.stringify({ version: 1, theme: 'dark' }),
    })
    const store = createSettingsStore(storage)
    expect(store.previewRoom.value).toBe(true)

    store.previewRoom.value = false
    expect(createSettingsStore(storage).previewRoom.value).toBe(false)

    // A non-boolean is rejected rather than coerced.
    expect(
      createSettingsStore(
        memoryStorage({
          'axon.settings': JSON.stringify({
            version: 1,
            previewRoom: 'yes',
          }),
        }),
      ).previewRoom.value,
    ).toBe(true)
  })

  it('appBadgeEnabled defaults to on and round-trips explicit off (ADR 0080)', () => {
    const storage = memoryStorage({
      'axon.settings': JSON.stringify({ version: 1, theme: 'dark' }),
    })
    const store = createSettingsStore(storage)
    expect(store.appBadgeEnabled.value).toBe(true)

    store.appBadgeEnabled.value = false
    expect(createSettingsStore(storage).appBadgeEnabled.value).toBe(false)

    // A non-boolean is rejected rather than coerced.
    expect(
      createSettingsStore(
        memoryStorage({
          'axon.settings': JSON.stringify({
            version: 1,
            appBadgeEnabled: 'yes',
          }),
        }),
      ).appBadgeEnabled.value,
    ).toBe(true)
  })

  it('messageComposerHeight defaults, validates, rounds, and round-trips', () => {
    const storage = memoryStorage({
      'axon.settings': JSON.stringify({
        version: 1,
        messageComposerHeight: 72.4,
      }),
    })
    const store = createSettingsStore(storage)
    expect(store.messageComposerHeight.value).toBe(72)

    store.messageComposerHeight.value = 96
    expect(createSettingsStore(storage).messageComposerHeight.value).toBe(96)

    for (const value of ['96', Number.NaN, 12]) {
      expect(
        createSettingsStore(
          memoryStorage({
            'axon.settings': JSON.stringify({
              version: 1,
              messageComposerHeight: value,
            }),
          }),
        ).messageComposerHeight.value,
      ).toBeNull()
    }
  })

  it('developerMode defaults to off and round-trips', () => {
    const storage = memoryStorage({
      'axon.settings': JSON.stringify({ version: 1, theme: 'dark' }),
    })
    const store = createSettingsStore(storage)
    expect(store.developerMode.value).toBe(false)

    store.developerMode.value = true
    expect(createSettingsStore(storage).developerMode.value).toBe(true)

    // A non-boolean is rejected rather than coerced.
    expect(
      createSettingsStore(
        memoryStorage({
          'axon.settings': JSON.stringify({
            version: 1,
            developerMode: 'yes',
          }),
        }),
      ).developerMode.value,
    ).toBe(false)
  })

  it('recent reactions default, validate, dedupe, cap, and persist', () => {
    const storage = memoryStorage({
      'axon.settings': JSON.stringify({
        version: 1,
        recentReactions: ['🔥', '', 42, '🦝'],
      }),
    })
    const store = createSettingsStore(storage)
    expect(store.recentReactions.value).toEqual(['🔥', '🦝'])

    store.recordRecentReaction('🔥')
    expect(store.recentReactions.value).toEqual(['🔥', '🦝'])

    store.recordRecentReaction('🚀')
    store.recordRecentReaction('⭐')
    expect(store.recentReactions.value).toEqual(['⭐', '🚀', '🔥'])
    expect(createSettingsStore(storage).recentReactions.value).toEqual(
      store.recentReactions.value,
    )
  })

  it('rejects invalid sort/filter values but keeps the rest', () => {
    const store = createSettingsStore(
      memoryStorage({
        'axon.settings': JSON.stringify({
          version: 1,
          theme: 'light',
          activeAccountId: null,
          pinnedRooms: ['a/x', 42, 'a/y'],
          roomSort: 'by-vibes',
          roomFilter: 'name',
        }),
      }),
    )
    expect(store.pinnedRooms.value).toEqual(['a/x', 'a/y'])
    expect(store.roomSort.value).toBe('recent')
    expect(store.roomFilter.value).toBe('all')
  })

  it('pinRoom prepends, re-pin moves to top, unpin removes', () => {
    const storage = memoryStorage()
    const store = createSettingsStore(storage)

    store.pinRoom('a/x')
    store.pinRoom('a/y')
    expect(store.pinnedRooms.value).toEqual(['a/y', 'a/x'])

    store.pinRoom('a/x') // re-pin → top (ADR 0038)
    expect(store.pinnedRooms.value).toEqual(['a/x', 'a/y'])

    store.unpinRoom('a/y')
    expect(store.pinnedRooms.value).toEqual(['a/x'])
    store.unpinRoom('a/z') // no-op
    expect(store.pinnedRooms.value).toEqual(['a/x'])

    // Persisted immediately.
    const reloaded = createSettingsStore(storage)
    expect(reloaded.pinnedRooms.value).toEqual(['a/x'])
  })

  it('sort and filter persist across stores', () => {
    const storage = memoryStorage()
    const first = createSettingsStore(storage)
    first.roomSort.value = 'az'
    first.roomFilter.value = 'dms'

    const second = createSettingsStore(storage)
    expect(second.roomSort.value).toBe('az')
    expect(second.roomFilter.value).toBe('dms')
  })
})

describe('applyTheme', () => {
  it('sets data-theme for explicit themes and removes it for system', () => {
    const store = createSettingsStore(memoryStorage())
    const root = document.createElement('html')
    const dispose = applyTheme(store, root)

    expect(root.hasAttribute('data-theme')).toBe(false)
    store.theme.value = 'dark'
    expect(root.getAttribute('data-theme')).toBe('dark')
    store.theme.value = 'light'
    expect(root.getAttribute('data-theme')).toBe('light')
    store.theme.value = 'system'
    expect(root.hasAttribute('data-theme')).toBe(false)
    dispose()
  })
})
