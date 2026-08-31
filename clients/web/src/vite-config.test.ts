import { describe, expect, it } from 'vitest'
import { resolveConfig } from 'vite'

describe('dev server watcher', () => {
  it('does not watch the Rust crate', async () => {
    // `src-tauri/` lives inside this package, so Vite's watcher walks into it
    // by default — `target/` alone is tens of thousands of files and gigabytes
    // of build output, none of it a frontend source.
    //
    // On Windows this is not merely wasteful: a running executable is locked,
    // so watching `target/debug/deps/axon_shell.exe` throws EBUSY and kills the
    // dev server. It presents intermittently — it depends on whether a previous
    // build's binary is still running when the watcher reaches that file — and
    // as "beforeDevCommand terminated with a non-zero status code", which names
    // nothing that looks like a cause.
    const config = await resolveConfig(
      { configFile: 'vite.config.ts' },
      'serve',
    )
    const ignored = config.server.watch?.ignored

    expect(ignored).toBeDefined()
    expect(
      (Array.isArray(ignored) ? ignored : [ignored]).some(
        (pattern) =>
          typeof pattern === 'string' && pattern.includes('src-tauri'),
      ),
    ).toBe(true)
  })
})
