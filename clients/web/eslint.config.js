import js from '@eslint/js'
import tseslint from 'typescript-eslint'
import reactHooks from 'eslint-plugin-react-hooks'
import prettier from 'eslint-config-prettier'
import globals from 'globals'

export default tseslint.config(
  { ignores: ['dist/', 'src/api/schema.d.ts'] },
  js.configs.recommended,
  tseslint.configs.recommended,
  {
    files: ['**/*.{ts,tsx}'],
    languageOptions: {
      globals: globals.browser,
    },
    plugins: {
      'react-hooks': reactHooks,
    },
    rules: {
      ...reactHooks.configs.recommended.rules,
      // Assumes React state semantics: it forbids writing to anything a hook
      // returned. This app's state is @preact/signals, whose *designed*
      // mutation idiom is `signal.value = x` — including signals reached
      // through the useServices() context hook.
      'react-hooks/immutability': 'off',
      // A debug `console.log` shipped to production in this app once, inside a
      // hot memo, stringifying a room's whole timeline slice on every recompute
      // — caught in review, not by any gate. `info`/`warn`/`error` stay allowed:
      // `reload.ts` and `app-badge.ts` use them as deliberate diagnostics.
      'no-console': ['error', { allow: ['info', 'warn', 'error'] }],
    },
  },
  {
    // Tests and e2e specs print freely; a stray log there costs nobody.
    files: ['**/*.test.{ts,tsx}', 'e2e/**', 'scripts/**'],
    rules: { 'no-console': 'off' },
  },
  {
    // The e2e lane, its config/mock, and local helper scripts run in Node, not
    // the browser.
    files: ['e2e/**', 'playwright.config.ts', 'scripts/**'],
    languageOptions: {
      globals: globals.node,
    },
  },
  prettier,
)
