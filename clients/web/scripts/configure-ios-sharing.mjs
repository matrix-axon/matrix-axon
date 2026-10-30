import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { execFileSync } from 'node:child_process'
import yaml from 'js-yaml'

// Run after EVERY tauri ios init. Generated Xcode files are never the source
// of truth. The optional probe is omitted from distribution builds by default.
const tauri = new URL('../src-tauri/', import.meta.url)
const apple = new URL('gen/apple/', tauri)
const entitlements = JSON.parse(
  execFileSync(
    'plutil',
    [
      '-convert',
      'json',
      '-o',
      '-',
      fileURLToPath(new URL('Entitlements.ios.plist', tauri)),
    ],
    { encoding: 'utf8' },
  ),
)
if (process.env.AXON_APS_ENVIRONMENT)
  entitlements['aps-environment'] = process.env.AXON_APS_ENVIRONMENT
const projectPath = new URL('project.yml', apple)
const project = yaml.load(readFileSync(projectPath, 'utf8'))
const app = project.targets.axon_iOS
app.info.properties.CFBundleShortVersionString = JSON.parse(
  readFileSync(new URL('../package.json', tauri), 'utf8'),
).version
// XcodeGen rewrites the entitlement plist from this block. Merely copying the
// committed plist before generation silently loses every capability.
app.entitlements = {
  path: 'axon_iOS/axon_iOS.entitlements',
  properties: entitlements,
}
app.info.properties.AxonSharedKeychainGroup =
  '$(AppIdentifierPrefix)org.matrixaxon.axon.sharing'
// Keep native sharing under default ATS; configured HTTP drafts remain local.
delete app.info.properties.NSAppTransportSecurity
app.settings.base = {
  ...app.settings.base,
  CODE_SIGN_ENTITLEMENTS: 'axon_iOS/axon_iOS.entitlements',
}
// Idempotence also removes the development-only probe if the next build does
// not opt in. It must never leak into an exported distribution IPA.
app.dependencies = app.dependencies.filter(
  (entry) => entry.target !== 'axon_share_probe',
)
delete project.targets.axon_share_probe
if (project.packages) delete project.packages.AxonIncomingShare
if (process.env.AXON_SHARE_PROBE === '1') {
  project.packages = {
    ...project.packages,
    AxonIncomingShare: { path: '../../incoming-share' },
  }
  project.targets.axon_share_probe = {
    type: 'app-extension',
    platform: 'iOS',
    deploymentTarget: '15.0',
    sources: ['../../share-probe'],
    dependencies: [{ package: 'AxonIncomingShare' }],
    settings: {
      base: {
        PRODUCT_BUNDLE_IDENTIFIER: 'org.matrixaxon.axon.ShareProbe',
        DEVELOPMENT_TEAM: project.settingGroups.app.base.DEVELOPMENT_TEAM,
        APPLICATION_EXTENSION_API_ONLY: 'YES',
        SKIP_INSTALL: 'YES',
        SWIFT_VERSION: '5.0',
      },
    },
    info: {
      path: 'ShareProbe/Info.plist',
      properties: {
        CFBundleDisplayName: 'Axon sharing probe',
        CFBundleShortVersionString:
          app.info.properties.CFBundleShortVersionString,
        CFBundleVersion: app.info.properties.CFBundleVersion,
        AxonSharedKeychainGroup:
          '$(AppIdentifierPrefix)org.matrixaxon.axon.sharing',
        NSExtension: {
          NSExtensionPointIdentifier: 'com.apple.share-services',
          NSExtensionPrincipalClass:
            '$(PRODUCT_MODULE_NAME).ShareProbeViewController',
          NSExtensionAttributes: {
            NSExtensionActivationRule: {
              NSExtensionActivationSupportsWebURLWithMaxCount: 1,
              NSExtensionActivationSupportsText: true,
            },
          },
        },
      },
    },
    entitlements: {
      path: 'ShareProbe/ShareProbe.entitlements',
      properties: {
        'com.apple.security.application-groups': [
          'group.org.matrixaxon.axon.sharing',
        ],
        'keychain-access-groups': [
          '$(AppIdentifierPrefix)org.matrixaxon.axon.sharing',
        ],
      },
    },
  }
  app.dependencies.push({ target: 'axon_share_probe', embed: true })
}
writeFileSync(projectPath, yaml.dump(project, { lineWidth: -1, noRefs: true }))
execFileSync('xcodegen', ['generate'], {
  cwd: fileURLToPath(apple),
  stdio: 'inherit',
})
