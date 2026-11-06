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

// Exercise the real generation hook in an isolated project. XcodeGen is
// replaced only after the hook has written its project and export settings.
test(
  'archive signing matches the export entitlement environment',
  { skip: process.platform !== 'darwin' },
  () => {
    const source = fileURLToPath(new URL('../', import.meta.url))
    for (const method of [
      'debugging',
      'app-store-connect',
      'release-testing',
    ]) {
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
            settingGroups: {
              app: { base: { DEVELOPMENT_TEAM: 'TEAM123456' } },
            },
            targets: {
              axon_iOS: {
                dependencies: [],
                settings: { base: {} },
                info: { properties: {} },
              },
            },
          }),
        )
        writeFileSync(join(bin, 'xcodegen'), '#!/bin/sh\nexit 0\n', {
          mode: 0o755,
        })
        const development = method === 'debugging'
        const appProfile = development ? 'app-development' : 'app-distribution'
        const shareProfile = development
          ? 'share-development'
          : 'share-distribution'
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
              AXON_IOS_EXPORT_METHOD: method,
              AXON_APS_ENVIRONMENT: development ? 'development' : 'production',
              AXON_IOS_APP_DEVELOPMENT_PROFILE: 'app-development',
              AXON_IOS_SHARE_DEVELOPMENT_PROFILE: 'share-development',
              AXON_IOS_APP_EXPORT_PROFILE: 'app-distribution',
              AXON_IOS_SHARE_EXPORT_PROFILE: 'share-distribution',
              PATH: bin + ':' + process.env.PATH,
            },
            stdio: 'pipe',
            timeout: 30_000,
          },
        )
        const project = yaml.load(
          readFileSync(join(apple, 'project.yml'), 'utf8'),
        )
        const options = JSON.parse(
          execFileSync(
            'plutil',
            ['-convert', 'json', '-o', '-', join(apple, 'ExportOptions.plist')],
            { encoding: 'utf8', timeout: 10_000 },
          ),
        )
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
            project.targets[target].settings.base
              .PROVISIONING_PROFILE_SPECIFIER,
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
      } finally {
        rmSync(root, { recursive: true, force: true })
      }
    }
  },
)
