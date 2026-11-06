import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import {
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { test } from 'node:test'
import yaml from 'js-yaml'

// Run the real generation hook in an isolated project and return what it wrote.
// XcodeGen is replaced only after the hook has written its project and export
// settings.
function generate(env) {
  const source = fileURLToPath(new URL('../', import.meta.url))
  const root = mkdtempSync(join(tmpdir(), 'axon-signing-contract-'))
  try {
    const web = join(root, 'clients/web')
    const scripts = join(web, 'scripts')
    const tauri = join(web, 'src-tauri')
    const apple = join(tauri, 'gen/apple')
    const bin = join(root, 'bin')
    for (const path of [scripts, apple, bin])
      mkdirSync(path, { recursive: true })
    symlinkSync(join(source, 'node_modules'), join(web, 'node_modules'))
    for (const name of [
      'configure-ios-sharing.mjs',
      'ios-share-activation.json',
    ])
      copyFileSync(join(source, 'scripts', name), join(scripts, name))
    for (const name of [
      'Entitlements.ios.plist',
      'Entitlements.share.ios.plist',
    ])
      copyFileSync(join(source, 'src-tauri', name), join(tauri, name))
    copyFileSync(join(source, 'package.json'), join(web, 'package.json'))
    writeFileSync(
      join(apple, 'project.yml'),
      JSON.stringify({
        settingGroups: { app: { base: { DEVELOPMENT_TEAM: 'TEAM123456' } } },
        targets: {
          axon_iOS: {
            dependencies: [],
            settings: { base: {} },
            info: { properties: {} },
          },
        },
      }),
    )
    writeFileSync(join(bin, 'xcodegen'), '#!/bin/sh\nexit 0\n', { mode: 0o755 })
    execFileSync(
      process.execPath,
      [join(scripts, 'configure-ios-sharing.mjs')],
      {
        env: {
          ...process.env,
          // Test the signed CI archive path, independently of API-key auth.
          APPLE_API_KEY: '',
          APPLE_API_ISSUER: '',
          APPLE_API_KEY_PATH: '',
          AXON_SHARE_PROBE: '',
          AXON_IOS_APP_DEVELOPMENT_PROFILE: 'app-development',
          AXON_IOS_SHARE_DEVELOPMENT_PROFILE: 'share-development',
          AXON_IOS_APP_EXPORT_PROFILE: 'app-distribution',
          AXON_IOS_SHARE_EXPORT_PROFILE: 'share-distribution',
          ...env,
          PATH: bin + ':' + process.env.PATH,
        },
        stdio: 'pipe',
        timeout: 30_000,
      },
    )
    const project = yaml.load(readFileSync(join(apple, 'project.yml'), 'utf8'))
    const options = JSON.parse(
      execFileSync(
        'plutil',
        ['-convert', 'json', '-o', '-', join(apple, 'ExportOptions.plist')],
        { encoding: 'utf8', timeout: 10_000 },
      ),
    )
    return { project, options }
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
}

test(
  'archive signing matches the export entitlement environment',
  { skip: process.platform !== 'darwin' },
  () => {
    for (const method of [
      'debugging',
      'app-store-connect',
      'release-testing',
    ]) {
      const development = method === 'debugging'
      const appProfile = development ? 'app-development' : 'app-distribution'
      const shareProfile = development
        ? 'share-development'
        : 'share-distribution'
      const { project, options } = generate({
        AXON_IOS_EXPORT_METHOD: method,
        AXON_APS_ENVIRONMENT: development ? 'development' : 'production',
      })
      for (const [target, profile] of [
        ['axon_iOS', appProfile],
        ['axon_share', shareProfile],
      ]) {
        assert.equal(
          project.targets[target].settings.base.CODE_SIGN_STYLE,
          'Manual',
          method,
        )
        assert.equal(
          project.targets[target].settings.base.CODE_SIGN_IDENTITY,
          development ? 'Apple Development' : 'Apple Distribution',
          method,
        )
        assert.equal(
          project.targets[target].settings.base.PROVISIONING_PROFILE_SPECIFIER,
          profile,
          method,
        )
      }
      assert.equal(
        project.targets.axon_iOS.entitlements.properties['aps-environment'],
        development ? 'development' : 'production',
      )
      assert.equal(
        options.provisioningProfiles['org.matrixaxon.axon'],
        appProfile,
      )
      assert.equal(
        options.provisioningProfiles['org.matrixaxon.axon.Share'],
        shareProfile,
      )
      assert.equal(
        options.signingCertificate,
        development ? 'Apple Development' : 'Apple Distribution',
      )
    }
  },
)

// Tauri archives unsigned when APPLE_API_* is set and signs only the app, so an
// extension with no signature of its own is exported without its App Group and
// shared Keychain. The post-build script is the whole fix; if it stops being
// generated, or points at the wrong file, the extension silently loses both again
// and only check-ios-sharing.py, after a full archive, notices.
test(
  'the extension and the probe carry their entitlements into an unsigned archive',
  { skip: process.platform !== 'darwin' },
  () => {
    const { project } = generate({
      AXON_SHARE_PROBE: '1',
      AXON_IOS_EXPORT_METHOD: 'debugging',
      AXON_APS_ENVIRONMENT: 'development',
    })
    for (const [name, path] of [
      ['axon_share', 'Share/Share.entitlements'],
      ['axon_share_probe', 'ShareProbe/ShareProbe.entitlements'],
    ]) {
      const target = project.targets[name]
      assert.ok(target, name)
      assert.equal(target.entitlements.path, path, name)
      assert.ok(
        target.postBuildScripts,
        `${name} has no post-build script: its signed entitlements would be dropped in an unsigned archive`,
      )
      assert.equal(target.postBuildScripts.length, 1, name)
      const [{ script }] = target.postBuildScripts
      // Only when Tauri disabled signing, and never for a signed build.
      assert.ok(
        script.includes('if [ "${CODE_SIGNING_ALLOWED}" = "NO" ]; then'),
        name,
      )
      // The variables must reach the shell as written, not be expanded (or
      // become "undefined") by the generator's own template literal.
      assert.ok(script.includes('${SRCROOT}/' + path + '"'), name)
      assert.ok(script.includes('"${CODESIGNING_FOLDER_PATH}"'), name)
      assert.ok(!script.includes('undefined'), name)
      assert.ok(
        script.includes('--sign - --timestamp=none --entitlements'),
        name,
      )
      assert.equal(
        target.postBuildScripts[0].basedOnDependencyAnalysis,
        false,
        name,
      )
      // What it signs with is what the target asks for.
      assert.deepEqual(
        target.entitlements.properties['com.apple.security.application-groups'],
        ['group.org.matrixaxon.axon.sharing'],
        name,
      )
      assert.deepEqual(
        target.entitlements.properties['keychain-access-groups'],
        ['$(AppIdentifierPrefix)org.matrixaxon.axon.sharing'],
        name,
      )
    }
  },
)
