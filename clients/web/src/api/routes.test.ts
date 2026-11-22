import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { API_ROUTES } from './routes'

describe('API_ROUTES', () => {
  it('lists every route in openapi.json', () => {
    // A route missing here still collapses safely in telemetry, keeping only
    // words some listed route uses; listing it keeps its parameter slots exact.
    const spec = JSON.parse(
      // From this file, not the cwd: the guard must hold however vitest runs.
      // (jsdom gives `import.meta.url` an http: scheme, so not via a URL.)
      readFileSync(
        resolve(import.meta.dirname, '../../../../openapi/openapi.json'),
        'utf8',
      ),
    ) as { paths: Record<string, unknown> }
    expect([...API_ROUTES].sort()).toEqual(Object.keys(spec.paths).sort())
  })
})
