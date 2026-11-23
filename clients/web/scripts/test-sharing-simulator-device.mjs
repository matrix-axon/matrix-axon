import assert from 'node:assert/strict'
import test from 'node:test'

import {
  createStandardPhone,
  pickPhone,
  rankDevice,
} from './sharing-simulator-device.mjs'

const device = (name, state = 'Shutdown') => ({
  name,
  state,
  udid: `udid-${name}`,
})

test('a standard model beats the Duo that sorted first under Xcode 27', () => {
  const picked = pickPhone([
    device('iPhone Duo'),
    device('iPhone 17'),
    device('iPhone 17 Pro'),
    device('iPhone Air'),
  ])
  assert.equal(picked.name, 'iPhone 17 Pro')
})

test('a Pro is preferred to the plain model, and either to a Pro Max or an Air', () => {
  assert.equal(
    pickPhone([device('iPhone 17 Pro Max'), device('iPhone 17')]).name,
    'iPhone 17',
  )
  assert.equal(
    pickPhone([device('iPhone Air'), device('iPhone 17 Pro Max')]).name,
    'iPhone 17 Pro Max',
  )
  assert.ok(rankDevice('iPhone 17 Pro') < rankDevice('iPhone 17'))
  assert.ok(rankDevice('iPhone 17') < rankDevice('iPhone 17 Pro Max'))
  assert.ok(rankDevice('iPhone 17 Pro Max') < rankDevice('iPhone Duo'))
})

test('among equally standard models, a booted one wins, then the newest', () => {
  assert.equal(
    pickPhone([device('iPhone 18 Pro'), device('iPhone 17 Pro', 'Booted')])
      .name,
    'iPhone 17 Pro',
  )
  assert.equal(
    pickPhone([device('iPhone 17 Pro'), device('iPhone 18 Pro')]).name,
    'iPhone 18 Pro',
  )
})

test('a booted exotic device does not beat a standard one', () => {
  assert.equal(
    pickPhone([device('iPhone Duo', 'Booted'), device('iPhone 17 Pro')]).name,
    'iPhone 17 Pro',
  )
})

test('with only unfamiliar models it still answers, and with none it does not', () => {
  assert.equal(pickPhone([device('iPhone Duo')]).name, 'iPhone Duo')
  assert.equal(pickPhone([]), undefined)
})

test('the order the devices arrive in does not matter', () => {
  const names = ['iPhone Duo', 'iPhone 17', 'iPhone Air', 'iPhone 17 Pro']
  for (const order of [names, [...names].reverse()])
    assert.equal(pickPhone(order.map((n) => device(n))).name, 'iPhone 17 Pro')
})

const quiet = { log: () => {}, warn: () => {} }
const runtimeId = 'com.apple.CoreSimulator.SimRuntime.iOS-27-1'
const listing = (names) =>
  JSON.stringify({
    runtimes: [
      {
        identifier: 'com.apple.CoreSimulator.SimRuntime.iOS-26-5',
        supportedDeviceTypes: [
          { name: 'iPhone 17 Pro', identifier: 'type.wrong-runtime' },
        ],
      },
      {
        identifier: runtimeId,
        supportedDeviceTypes: names.map((name) => ({
          name,
          identifier: `type.${name.replaceAll(' ', '-')}`,
        })),
      },
    ],
  })

test('creates the most standard model the runtime supports, on that runtime', () => {
  const calls = []
  const made = createStandardPhone({
    runtime: runtimeId,
    log: quiet,
    exec: (args) => {
      calls.push(args)
      return args[0] === 'list'
        ? listing(['iPhone Duo', 'iPhone 17', 'iPhone 17 Pro', 'iPad Pro'])
        : 'NEW-UDID\n'
    },
  })
  assert.deepEqual(calls[1], [
    'create',
    'Axon share tests (iPhone 17 Pro)',
    'type.iPhone-17-Pro',
    runtimeId,
  ])
  assert.deepEqual(made, {
    name: 'iPhone 17 Pro',
    udid: 'NEW-UDID',
    state: 'Shutdown',
    runtime: runtimeId,
  })
})

test('creates nothing when the runtime supports only unfamiliar models', () => {
  const calls = []
  const made = createStandardPhone({
    runtime: runtimeId,
    log: quiet,
    exec: (args) => {
      calls.push(args[0])
      return listing(['iPhone Duo'])
    },
  })
  assert.equal(made, undefined)
  assert.deepEqual(calls, ['list'])
})

test('a simctl that refuses to create, or to list, leaves the caller its own device', () => {
  const warned = []
  const refusing = createStandardPhone({
    runtime: runtimeId,
    log: { log: () => {}, warn: (message) => warned.push(message) },
    exec: (args) => {
      if (args[0] === 'create') throw new Error('Invalid device type')
      return listing(['iPhone 17 Pro'])
    },
  })
  assert.equal(refusing, undefined)
  assert.match(warned[0], /Invalid device type/)
  assert.equal(
    createStandardPhone({
      runtime: runtimeId,
      log: quiet,
      exec: () => {
        throw new Error('no simctl')
      },
    }),
    undefined,
  )
})
