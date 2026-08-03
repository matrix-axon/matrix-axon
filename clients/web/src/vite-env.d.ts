/// <reference types="vite/client" />

declare const __AXON_WEB_RELEASE__: string
declare const __AXON_WEB_VERSION__: string
declare const __AXON_WEB_BUILT_AT__: string

interface ImportMetaEnv {
  /** Cross-origin server base for separately-hosted deployments (M-W1.5). */
  readonly VITE_AXON_SERVER_URL?: string
  readonly VITE_AXON_OAUTH_CLIENT_ID?: string
  readonly VITE_AXON_OAUTH_PROVIDERS?: string
  /** OAuth callback origin override; same-origin if unset (M-W12 Tauri prep). */
  readonly VITE_AXON_OAUTH_REDIRECT_URI?: string
}

interface ImportMeta {
  readonly env: ImportMetaEnv
}

// Generated at build time by the `axon-thirdparty-licenses` plugin in
// vite.config.ts from the pnpm production dependency tree.
declare module 'virtual:thirdparty-licenses' {
  import type { ThirdPartyLicense } from './thirdparty-disclosure'
  const licenses: ThirdPartyLicense[]
  export default licenses
}
