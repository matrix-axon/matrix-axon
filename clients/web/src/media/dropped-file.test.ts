import { describe, expect, it } from 'vitest'
import { fileFromPath, mediaTypeForPath } from './dropped-file'

describe('mediaTypeForPath', () => {
  it('types an image, case-insensitively', () => {
    expect(mediaTypeForPath('/tmp/a.JPG')).toBe('image/jpeg')
    expect(mediaTypeForPath('/tmp/a.png')).toBe('image/png')
  })

  it('types video, audio and documents', () => {
    expect(mediaTypeForPath('/tmp/clip.mp4')).toBe('video/mp4')
    expect(mediaTypeForPath('/tmp/voice.ogg')).toBe('audio/ogg')
    expect(mediaTypeForPath('/tmp/notes.pdf')).toBe('application/pdf')
  })

  it('gives no type to an unknown extension', () => {
    // '' is what a browser reports for a file it cannot type, and the send
    // path already omits `mimetype` for it rather than asserting a wrong one.
    expect(mediaTypeForPath('/tmp/archive.rar')).toBe('')
    expect(mediaTypeForPath('/tmp/README')).toBe('')
  })

  it('does not read a leading dot as an extension', () => {
    expect(mediaTypeForPath('/home/adam/.gitignore')).toBe('')
  })

  it('ignores a dot in a directory above the file', () => {
    expect(mediaTypeForPath('/home/adam/photos.old/holiday')).toBe('')
  })
})

describe('fileFromPath', () => {
  it('names and types the file from its path', async () => {
    const bytes = new Uint8Array([1, 2, 3]).buffer
    const file = fileFromPath('/home/adam/holiday.jpg', bytes)
    expect(file.name).toBe('holiday.jpg')
    // Load-bearing: this is what decides `m.image` over `m.file`, and it
    // becomes the upload's Content-Type.
    expect(file.type).toBe('image/jpeg')
    expect(new Uint8Array(await file.arrayBuffer())).toEqual(
      new Uint8Array([1, 2, 3]),
    )
  })
})
