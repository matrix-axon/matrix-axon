import { beforeEach, describe, expect, it } from 'vitest'
import { memoryStorage } from '../test/memory-storage'
import {
  deliverNotificationClick,
  messageNotificationBody,
  notificationClickFromPayload,
  notificationPermissionState,
  readNotificationTargets,
  rememberNotificationTarget,
  subscribeNotificationClicks,
  type NotificationClick,
} from './notifications'

beforeEach(() => {
  const stop = subscribeNotificationClicks(() => {})
  stop()
})

describe('messageNotificationBody', () => {
  it('names the sender and clips a long body', () => {
    const body = messageNotificationBody('@alice', 'a'.repeat(200))
    expect(body.startsWith('@alice: ')).toBe(true)
    expect(body.endsWith('…')).toBe(true)
    // 139 characters of the body plus the ellipsis.
    expect(body.length).toBe('@alice: '.length + 140)
  })

  it('keeps a message that has no text', () => {
    expect(messageNotificationBody('@alice', '  ')).toBe('@alice')
    expect(messageNotificationBody('', null)).toBe('New message')
  })
})

describe('notificationClickFromPayload', () => {
  const targets = [
    {
      id: 7,
      accountId: 'acct',
      roomId: '!room:server',
      eventId: '$evt',
      threadRootId: '$root',
    },
  ]

  it('reads the extra payload Android returns around the notification', () => {
    expect(
      notificationClickFromPayload(
        {
          actionId: 'tap',
          notification: {
            id: 7,
            extra: {
              accountId: 'acct',
              roomId: '!room:server',
              eventId: '$evt',
              threadRootId: '$root',
            },
          },
        },
        [],
      ),
    ).toEqual({
      accountId: 'acct',
      roomId: '!room:server',
      eventId: '$evt',
      threadRootId: '$root',
    })
  })

  it('resolves an iOS tap, which carries an id and no extra, from the remembered post', () => {
    expect(
      notificationClickFromPayload(
        { actionId: 'tap', notification: { id: '7' } },
        targets,
      ),
    ).toEqual({
      accountId: 'acct',
      roomId: '!room:server',
      eventId: '$evt',
      threadRootId: '$root',
    })
  })

  it('ignores a dismiss', () => {
    expect(
      notificationClickFromPayload(
        {
          actionId: 'dismiss',
          extra: { accountId: 'acct', roomId: '!room:server' },
        },
        targets,
      ),
    ).toBeNull()
  })
})

describe('rememberNotificationTarget', () => {
  it('round-trips the id it assigns', () => {
    const storage = memoryStorage()
    const id = rememberNotificationTarget(storage, {
      accountId: 'acct',
      roomId: '!room:server',
      eventId: '$evt',
      threadRootId: null,
    })
    expect(readNotificationTargets(storage)).toEqual([
      {
        id,
        accountId: 'acct',
        roomId: '!room:server',
        eventId: '$evt',
        threadRootId: null,
      },
    ])
  })
})

describe('notification taps', () => {
  it('holds a tap until a listener exists, then delivers it', () => {
    deliverNotificationClick({
      accountId: 'acct',
      roomId: '!room:server',
      eventId: '$evt',
      threadRootId: null,
    })
    const seen: NotificationClick[] = []
    const stop = subscribeNotificationClicks((click) => seen.push(click))
    expect(seen).toEqual([
      {
        accountId: 'acct',
        roomId: '!room:server',
        eventId: '$evt',
        threadRootId: null,
      },
    ])
    stop()
  })
})

describe('notificationPermissionState', () => {
  it('folds the plugin prompt states into default', () => {
    expect(notificationPermissionState('granted')).toBe('granted')
    expect(notificationPermissionState('denied')).toBe('denied')
    expect(notificationPermissionState('prompt')).toBe('default')
    expect(notificationPermissionState('prompt-with-rationale')).toBe('default')
    expect(notificationPermissionState('unsupported')).toBe('unsupported')
  })
})
