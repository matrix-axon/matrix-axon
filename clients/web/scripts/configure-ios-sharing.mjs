import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { execFileSync } from 'node:child_process'
import yaml from 'js-yaml'

// Run after EVERY tauri ios init. Generated Xcode files are never the source
// of truth. The production share destination is always embedded; the optional
// probe is omitted from distribution builds by default.
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
// Info.plist is expanded while compiling, before a profile is resolved, so
// $(AppIdentifierPrefix) can come out empty there; the entitlements are
// expanded at signing and are fine. The team id is a project setting.
app.info.properties.AxonSharedKeychainGroup =
  '$(DEVELOPMENT_TEAM).org.matrixaxon.axon.sharing'
// Keep native sharing under default ATS; configured HTTP drafts remain local.
delete app.info.properties.NSAppTransportSecurity
app.settings.base = {
  ...app.settings.base,
  CODE_SIGN_ENTITLEMENTS: 'axon_iOS/axon_iOS.entitlements',
}
for (const key of [
  'CODE_SIGN_STYLE',
  'CODE_SIGN_IDENTITY',
  'PROVISIONING_PROFILE_SPECIFIER',
])
  delete app.settings.base[key]
// Idempotence also removes the development-only probe if the next build does
// not opt in. It must never leak into an exported distribution IPA.
app.dependencies = app.dependencies.filter(
  (entry) => !['axon_share_probe', 'axon_share'].includes(entry.target),
)
delete project.targets.axon_share_probe
delete project.targets.axon_share
{
  project.packages = {
    ...project.packages,
    AxonIncomingShare: { path: '../../incoming-share' },
  }
  const extension = {
    type: 'app-extension',
    platform: 'iOS',
    deploymentTarget: '15.0',
    sources: ['../../share-extension'],
    dependencies: [{ package: 'AxonIncomingShare' }],
    settings: {
      base: {
        PRODUCT_BUNDLE_IDENTIFIER: 'org.matrixaxon.axon.Share',
        DEVELOPMENT_TEAM: project.settingGroups.app.base.DEVELOPMENT_TEAM,
        APPLICATION_EXTENSION_API_ONLY: 'YES',
        SKIP_INSTALL: 'YES',
        SWIFT_VERSION: '5.0',
      },
    },
    info: {
      path: 'Share/Info.plist',
      properties: {
        CFBundleDisplayName: 'Axon',
        CFBundleShortVersionString:
          app.info.properties.CFBundleShortVersionString,
        CFBundleVersion: app.info.properties.CFBundleVersion,
        AxonSharedKeychainGroup:
          '$(DEVELOPMENT_TEAM).org.matrixaxon.axon.sharing',
        NSExtension: {
          NSExtensionPointIdentifier: 'com.apple.share-services',
          NSExtensionPrincipalClass:
            '$(PRODUCT_MODULE_NAME).ShareViewController',
          NSExtensionAttributes: {
            NSExtensionActivationRule: JSON.parse(
              readFileSync(
                new URL('ios-share-activation.json', import.meta.url),
                'utf8',
              ),
            ),
          },
        },
      },
    },
    entitlements: {
      path: 'Share/Share.entitlements',
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
  project.targets.axon_share = extension
  app.dependencies.push({ target: 'axon_share', embed: true })
  if (process.env.AXON_SHARE_PROBE === '1') {
    const probe = structuredClone(extension)
    probe.sources = ['../../share-probe']
    probe.settings.base.PRODUCT_BUNDLE_IDENTIFIER =
      'org.matrixaxon.axon.ShareProbe'
    probe.info.path = 'ShareProbe/Info.plist'
    probe.info.properties.CFBundleDisplayName = 'Axon sharing probe'
    probe.info.properties.NSExtension.NSExtensionPrincipalClass =
      '$(PRODUCT_MODULE_NAME).ShareProbeViewController'
    probe.entitlements.path = 'ShareProbe/ShareProbe.entitlements'
    project.targets.axon_share_probe = probe
    app.dependencies.push({ target: 'axon_share_probe', embed: true })
  }
}
// Xcode automatic signing ignores some API-created profiles even when the
// installed profile grants every required entitlement. Allow explicit archive
// selection without changing the optional probe's signing identity.
const shareDevelopmentProfile = process.env.AXON_IOS_SHARE_DEVELOPMENT_PROFILE
const exportPath = new URL('ExportOptions.plist', apple)
const exportOptions = existsSync(exportPath)
  ? JSON.parse(
      execFileSync(
        'plutil',
        ['-convert', 'json', '-o', '-', fileURLToPath(exportPath)],
        { encoding: 'utf8' },
      ),
    )
  : {}
const exportMethod =
  process.env.AXON_IOS_EXPORT_METHOD ?? exportOptions.method ?? 'debugging'
exportOptions.method = exportMethod
if (shareDevelopmentProfile) {
  const appDevelopmentProfile = process.env.AXON_IOS_APP_DEVELOPMENT_PROFILE
  if (!appDevelopmentProfile)
    throw new Error(
      'Explicit signing also requires AXON_IOS_APP_DEVELOPMENT_PROFILE',
    )
  if (
    shareDevelopmentProfile.length > 256 ||
    Array.from(shareDevelopmentProfile).some(
      (character) =>
        character.charCodeAt(0) < 32 || character.charCodeAt(0) === 127,
    )
  )
    throw new Error(
      'AXON_IOS_SHARE_DEVELOPMENT_PROFILE must be a profile name or UUID',
    )
  Object.assign(project.targets.axon_share.settings.base, {
    CODE_SIGN_STYLE: 'Manual',
    CODE_SIGN_IDENTITY: 'Apple Development',
    PROVISIONING_PROFILE_SPECIFIER: shareDevelopmentProfile,
  })
  // Tauri recomputes signingStyle from the containing target after reading
  // ExportOptions.plist. Both targets must declare the same manual policy.
  Object.assign(app.settings.base, {
    CODE_SIGN_STYLE: 'Manual',
    CODE_SIGN_IDENTITY: 'Apple Development',
    PROVISIONING_PROFILE_SPECIFIER: appDevelopmentProfile,
  })
  const development = exportMethod === 'debugging'
  const store = exportMethod === 'app-store-connect'
  const prefix = development
    ? 'iOS Team Provisioning Profile: '
    : store
      ? 'iOS Team Store Provisioning Profile: '
      : null
  const appProfile =
    process.env.AXON_IOS_APP_EXPORT_PROFILE ??
    (development
      ? appDevelopmentProfile
      : prefix && prefix + 'org.matrixaxon.axon')
  const shareProfile =
    process.env.AXON_IOS_SHARE_EXPORT_PROFILE ??
    (development
      ? shareDevelopmentProfile
      : prefix && prefix + 'org.matrixaxon.axon.Share')
  if (!appProfile || !shareProfile)
    throw new Error(
      'Explicit export profiles are required for this export method',
    )
  Object.assign(exportOptions, {
    signingStyle: 'manual',
    signingCertificate: development
      ? 'Apple Development'
      : 'Apple Distribution',
    teamID: project.settingGroups.app.base.DEVELOPMENT_TEAM,
    provisioningProfiles: {
      'org.matrixaxon.axon': appProfile,
      'org.matrixaxon.axon.Share': shareProfile,
      ...(project.targets.axon_share_probe
        ? {
            'org.matrixaxon.axon.ShareProbe':
              process.env.AXON_IOS_SHARE_PROBE_EXPORT_PROFILE ??
              'iOS Team Provisioning Profile: org.matrixaxon.axon.ShareProbe',
          }
        : {}),
    },
  })
} else {
  for (const key of [
    'signingStyle',
    'signingCertificate',
    'teamID',
    'provisioningProfiles',
  ])
    delete exportOptions[key]
}
execFileSync(
  'plutil',
  ['-convert', 'xml1', '-o', fileURLToPath(exportPath), '-'],
  { input: JSON.stringify(exportOptions) },
)
writeFileSync(projectPath, yaml.dump(project, { lineWidth: -1, noRefs: true }))
execFileSync('xcodegen', ['generate'], {
  cwd: fileURLToPath(apple),
  stdio: 'inherit',
})
