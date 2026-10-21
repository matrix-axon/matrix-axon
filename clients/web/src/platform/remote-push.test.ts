import { describe, expect, it } from 'vitest'
import { memoryStorage } from '../test/memory-storage'
import {
  acceptWake,
  loadSeenWakes,
  releaseRetainedTwin,
  rememberSeenWake,
  type SeenWakeGate,
} from './remote-push'

function gate(storage: Storage): SeenWakeGate {
  return { seen: loadSeenWakes(storage), suppressLive: null }
}

describe('seen push wakes', () => {
  it('remembers a wake across a fresh read of the same store', () => {
    const storage = memoryStorage()
    const seen = loadSeenWakes(storage)
    rememberSeenWake(storage, seen, '$event')
    expect(loadSeenWakes(storage)).toEqual(['$event'])
    rememberSeenWake(storage, seen, '$event')
    expect(loadSeenWakes(storage)).toEqual(['$event'])
  })

  it('ignores the retained replay and then opens a later tap', () => {
    const storage = memoryStorage()
    const first = gate(storage)
    expect(acceptWake(storage, first, '$event', 'live')).toBe(true)
    expect(loadSeenWakes(storage)).toEqual(['$event'])

    const reloaded = gate(storage)
    expect(acceptWake(storage, reloaded, '$event', 'retained')).toBe(false)
    expect(loadSeenWakes(storage)).toEqual([])
    expect(acceptWake(storage, reloaded, '$event', 'live')).toBe(true)
  })

  it('opens a live wake once when the retained copy follows in this document', () => {
    const storage = memoryStorage()
    const current = gate(storage)
    expect(acceptWake(storage, current, '$event', 'live')).toBe(true)
    expect(acceptWake(storage, current, '$event', 'retained')).toBe(false)
    expect(acceptWake(storage, current, '$event', 'live')).toBe(true)
  })

  it('opens a cold-start wake once and does not keep it for the session', () => {
    const storage = memoryStorage()
    const current = gate(storage)
    expect(acceptWake(storage, current, '$event', 'retained')).toBe(true)
    expect(loadSeenWakes(storage)).toEqual([])
    expect(acceptWake(storage, current, '$event', 'live')).toBe(false)
    releaseRetainedTwin(current)
    expect(acceptWake(storage, current, '$event', 'live')).toBe(true)
    expect(loadSeenWakes(storage)).toEqual(['$event'])
  })
})
