import assert from 'node:assert/strict'
import test from 'node:test'

import { pickPhone, rankDevice } from './sharing-simulator-device.mjs'

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
