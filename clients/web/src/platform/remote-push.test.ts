import { describe, expect, it } from 'vitest'
import { memoryStorage } from '../test/memory-storage'
import { loadSeenWakes, rememberSeenWake } from './remote-push'

describe('seen push wakes', () => {
  it('remembers a wake across a fresh read of the same store', () => {
    const storage = memoryStorage()
    const seen = loadSeenWakes(storage)
    rememberSeenWake(storage, seen, '$event')
    expect(loadSeenWakes(storage)).toEqual(['$event'])
    rememberSeenWake(storage, seen, '$event')
    expect(loadSeenWakes(storage)).toEqual(['$event'])
  })
})
