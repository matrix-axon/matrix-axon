import { existsSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { isEntryPoint } from './is-entry-point.mjs'

// The oldest iOS the app is built to run on: `bundle.iOS.minimumSystemVersion` in
// tauri.conf.json, which tauri.ios.conf.json may override, the way Tauri layers them.
// One value, read here, is what the generated project declares for the app (see
// configure-ios-sharing.mjs) and what the build is checked against afterwards.
//
// It has to be read and applied explicitly. `tauri ios init` writes it to the project
// as `options.deploymentTarget`, which xcodegen does not carry into the Xcode project,
// so the app target declared nothing and took the SDK's version: 26.5 under Xcode 26,
// 27.2 under Xcode 27, and the next release's under the next. The share extension, which
// sets its own, was always right.

const read = (path) =>
  existsSync(path) ? JSON.parse(readFileSync(path, 'utf8')) : {}

/** "26.0" style only, so a built app's MinimumOSVersion can be compared as text. */
export const isVersion = (value) => /^\d+\.\d+$/.test(value)

/** `tauriDir` is the src-tauri directory. Throws when no valid version is configured. */
export const minimumIosVersion = (tauriDir) => {
  const base = read(join(tauriDir, 'tauri.conf.json'))
  const ios = read(join(tauriDir, 'tauri.ios.conf.json'))
  const version =
    ios.bundle?.iOS?.minimumSystemVersion ??
    base.bundle?.iOS?.minimumSystemVersion
  if (typeof version !== 'string' || !isVersion(version))
    throw new Error(
      `bundle.iOS.minimumSystemVersion must be a "major.minor" string such as "26.0"; got ${JSON.stringify(version)}`,
    )
  return version
}

// `node ios-minimum-version.mjs` prints it, for the shell scripts that check a build.
if (isEntryPoint(import.meta.url)) {
  try {
    console.log(
      minimumIosVersion(
        fileURLToPath(new URL('../src-tauri/', import.meta.url)),
      ),
    )
  } catch (error) {
    console.error(`error: ${error.message}`)
    process.exit(2)
  }
}
