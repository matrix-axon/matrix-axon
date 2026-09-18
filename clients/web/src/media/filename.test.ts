import { describe, expect, it } from 'vitest'
import { basename } from './filename'

describe('basename', () => {
  it('takes the last segment of a posix path', () => {
    expect(basename('/home/adam/holiday.jpg')).toBe('holiday.jpg')
  })

  it('takes the last segment of a windows path', () => {
    // Checked on every platform, not just Windows: a name that still carried
    // `C:\Users\...` would be shown to the user and sent to the room as the
    // filename.
    expect(basename('C:\\Users\\adam\\holiday.jpg')).toBe('holiday.jpg')
  })

  it('leaves a bare name alone', () => {
    expect(basename('holiday.jpg')).toBe('holiday.jpg')
  })

  it('drops the directory from a traversal-shaped name', () => {
    // `content.filename` is whatever the sender put there. `<a download>`
    // dropped the directory; the save dialog must not get it back.
    expect(basename('../../.config/autostart/evil.desktop')).toBe(
      'evil.desktop',
    )
  })

  it('falls back when nothing is left to call the file', () => {
    // A save dialog handed `''` has nothing to offer, and `..` is not a name.
    expect(basename('../')).toBe('download')
    expect(basename('/tmp/dir/')).toBe('download')
    expect(basename('..')).toBe('download')
    expect(basename('   ')).toBe('download')
    expect(basename('', 'untitled')).toBe('untitled')
  })
})
