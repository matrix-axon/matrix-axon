import assert from 'node:assert/strict'
import {
  mkdirSync,
  mkdtempSync,
  realpathSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { pathToFileURL } from 'node:url'

import { isEntryPoint } from './is-entry-point.mjs'

// A real directory holding a script, and a symlink to the directory and one to the file.
const fixture = () => {
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'is-entry-point-test-')))
  const real = join(root, 'real')
  mkdirSync(real)
  const file = join(real, 'entry.mjs')
  writeFileSync(file, '// a script\n')
  writeFileSync(join(real, 'other.mjs'), '// another script\n')
  symlinkSync(real, join(root, 'dir-link'))
  symlinkSync(file, join(root, 'file-link.mjs'))
  // Node gives a module the URL of its resolved path, so that is what is compared.
  return { root, file, moduleUrl: pathToFileURL(file).href }
}

test('is true for the file started directly and through a symlink to it or to its folder', () => {
  const { root, file, moduleUrl } = fixture()
  assert.equal(isEntryPoint(moduleUrl, file), true)
  assert.equal(isEntryPoint(moduleUrl, join(root, 'file-link.mjs')), true)
  assert.equal(
    isEntryPoint(moduleUrl, join(root, 'dir-link', 'entry.mjs')),
    true,
  )
  rmSync(root, { recursive: true })
})

test('is false for another file, a missing one, and no entry at all', () => {
  const { root, moduleUrl } = fixture()
  assert.equal(isEntryPoint(moduleUrl, join(root, 'real', 'other.mjs')), false)
  assert.equal(isEntryPoint(moduleUrl, join(root, 'nowhere.mjs')), false)
  assert.equal(isEntryPoint(moduleUrl, ''), false)
  assert.equal(isEntryPoint(moduleUrl, undefined), false)
  rmSync(root, { recursive: true })
})
