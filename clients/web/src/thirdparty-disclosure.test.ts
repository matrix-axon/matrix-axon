import { describe, expect, it, vi } from 'vitest'
import {
  buildDisclosure,
  collectDisclosure,
  pickLicenseFile,
  pnpmCommand,
} from './thirdparty-disclosure'

describe('pickLicenseFile', () => {
  it('returns null when no license-like file is present', () => {
    expect(
      pickLicenseFile(['index.js', 'package.json', 'readme.md']),
    ).toBeNull()
  })

  it('prefers a bare LICENSE over an extensioned one, regardless of order', () => {
    expect(pickLicenseFile(['LICENSE.md', 'LICENSE'])).toBe('LICENSE')
    expect(pickLicenseFile(['LICENSE', 'LICENSE.md'])).toBe('LICENSE')
  })

  it('ranks LICENSE over COPYING over NOTICE', () => {
    expect(pickLicenseFile(['NOTICE', 'COPYING', 'LICENSE'])).toBe('LICENSE')
    expect(pickLicenseFile(['NOTICE', 'COPYING'])).toBe('COPYING')
  })

  it('is deterministic for dual-licensed packages via an alphabetical tie-break', () => {
    // Same rank (both extensioned LICENSE variants) → stable alphabetical pick,
    // independent of readdir order.
    expect(pickLicenseFile(['LICENSE-MIT', 'LICENSE-APACHE'])).toBe(
      'LICENSE-APACHE',
    )
    expect(pickLicenseFile(['LICENSE-APACHE', 'LICENSE-MIT'])).toBe(
      'LICENSE-APACHE',
    )
  })

  it('matches case-insensitively (LICENCE spelling, COPYING.txt)', () => {
    expect(pickLicenseFile(['licence'])).toBe('licence')
    expect(pickLicenseFile(['copying.txt'])).toBe('copying.txt')
  })
})

describe('buildDisclosure', () => {
  const raw = JSON.stringify({
    MIT: [
      {
        name: 'beta',
        versions: ['2.0.0'],
        paths: ['/pkgs/beta'],
        license: 'MIT',
      },
      {
        name: 'alpha',
        versions: ['1.0.0'],
        paths: ['/pkgs/alpha'],
        license: 'MIT',
      },
    ],
    ISC: [{ name: 'gamma', versions: ['3.0.0'], license: 'ISC' }],
  })

  it('sorts by name and reads license text via the injected reader', () => {
    const result = buildDisclosure(raw, (dir) => `TEXT for ${dir}`)
    expect(result.map((p) => p.name)).toEqual(['alpha', 'beta', 'gamma'])
    expect(result[0]).toMatchObject({
      name: 'alpha',
      version: '1.0.0',
      license: 'MIT',
      text: 'TEXT for /pkgs/alpha',
    })
  })

  it('falls back to a declared-license note when no text is available', () => {
    const result = buildDisclosure(raw, () => null)
    const gamma = result.find((p) => p.name === 'gamma')
    // gamma has no `paths`, so the reader is never consulted.
    expect(gamma?.text).toBe(
      'No license file was bundled with this package. Declared license: ISC.',
    )
  })

  it('de-duplicates repeated name@version entries', () => {
    const dup = JSON.stringify({
      MIT: [
        { name: 'alpha', versions: ['1.0.0'], paths: ['/a'], license: 'MIT' },
        {
          name: 'alpha',
          versions: ['1.0.0'],
          paths: ['/a-copy'],
          license: 'MIT',
        },
      ],
    })
    const result = buildDisclosure(dup, (dir) => dir)
    expect(result).toHaveLength(1)
    expect(result[0].text).toBe('/a') // first wins
  })

  it('throws on non-object JSON so callers can degrade', () => {
    expect(() => buildDisclosure('"a string"', () => null)).toThrow()
    expect(() => buildDisclosure('not json at all', () => null)).toThrow()
  })
})

describe('collectDisclosure', () => {
  const readLicense = () => 'LICENSE TEXT'

  it('degrades to an empty list and warns when the pnpm call fails', () => {
    const warn = vi.fn()
    const result = collectDisclosure(
      () => {
        throw new Error('pnpm not found')
      },
      readLicense,
      warn,
    )
    expect(result).toEqual([])
    expect(warn).toHaveBeenCalledOnce()
  })

  it('degrades to an empty list and warns when the output is not valid JSON', () => {
    const warn = vi.fn()
    const result = collectDisclosure(
      () => 'WARN: something\n{bad',
      readLicense,
      warn,
    )
    expect(result).toEqual([])
    expect(warn).toHaveBeenCalledOnce()
  })

  it('returns the parsed disclosure on success', () => {
    const warn = vi.fn()
    const raw = JSON.stringify({
      MIT: [
        { name: 'alpha', versions: ['1.0.0'], paths: ['/a'], license: 'MIT' },
      ],
    })
    const result = collectDisclosure(() => raw, readLicense, warn)
    expect(result).toHaveLength(1)
    expect(result[0].name).toBe('alpha')
    expect(warn).not.toHaveBeenCalled()
  })
})

describe('pnpmCommand', () => {
  it('runs a JS entry point with the current node, so no shim is needed', () => {
    expect(
      pnpmCommand('C:\\Users\\x\\setup-pnpm\\pnpm.cjs', 'C:\\node\\node.exe'),
    ).toEqual({
      command: 'C:\\node\\node.exe',
      args: ['C:\\Users\\x\\setup-pnpm\\pnpm.cjs'],
    })
  })

  it('runs a .mjs entry point with the current node', () => {
    expect(pnpmCommand('/opt/pnpm/bin/pnpm.mjs', '/usr/bin/node')).toEqual({
      command: '/usr/bin/node',
      args: ['/opt/pnpm/bin/pnpm.mjs'],
    })
  })

  it('runs pnpm.exe directly', () => {
    expect(pnpmCommand('C:\\pnpm\\pnpm.exe', 'C:\\node\\node.exe')).toEqual({
      command: 'C:\\pnpm\\pnpm.exe',
      args: [],
    })
  })

  it('does not trust a wrapper that merely starts with pnpm', () => {
    expect(pnpmCommand('/opt/bin/pnpm-wrapper.sh', '/usr/bin/node')).toEqual({
      command: 'pnpm',
      args: [],
    })
  })

  it('runs a native pnpm binary directly', () => {
    expect(pnpmCommand('/usr/local/bin/pnpm', '/usr/bin/node')).toEqual({
      command: '/usr/local/bin/pnpm',
      args: [],
    })
  })

  it('falls back to pnpm when not run by pnpm', () => {
    const fallback = { command: 'pnpm', args: [] }
    expect(pnpmCommand(undefined, '/usr/bin/node')).toEqual(fallback)
    expect(pnpmCommand('C:\\\\p\\\\pnpm.cmd', 'node')).toEqual(fallback)
    expect(
      pnpmCommand('/usr/lib/node_modules/npm/bin/npm-cli.js', '/usr/bin/node'),
    ).toEqual(fallback)
  })
})
