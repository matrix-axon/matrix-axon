# Axon documentation

The [project README](../README.md) is the short introduction.
This page indexes everything else.

## Use Axon

| Guide                                                      | What it covers                                                                   |
| ---------------------------------------------------------- | -------------------------------------------------------------------------------- |
| [Installing Axon](install.md)                              | Server and clients, per operating system                                         |
| [Docker guide](../deploy/README.md)                        | The Compose stack: quick start, Tailscale and Caddy profiles, volumes and backup |
| [Administering the server](server-admin.md)                | Sign-in, tokens, SSO, adding accounts, push, management API, TLS                 |
| [Technical reference](technical-reference.md)              | Architecture, database encoding and deadlines, environment variables and config  |
| [axon-tui](../clients/tui/README.md)                       | The terminal client: commands, shortcuts, configuration                          |
| [axon-web](../clients/web/README.md)                       | The web client                                                                   |
| [Desktop app](../clients/web/src-tauri/README.md)          | The Tauri shell for macOS, Windows and Linux                                     |
| [Room metadata API](room-metadata.md)                      | Cached room-state details exposed to clients                                     |
| [Support](support.md), [Privacy policy](PRIVACY_POLICY.md) | Getting help; what data Axon handles                                             |

## Build and contribute

| Guide                                         | What it covers                                            |
| --------------------------------------------- | --------------------------------------------------------- |
| [Building from source](building.md)           | Prerequisites, first-time setup, running, troubleshooting |
| [CONTRIBUTING](../CONTRIBUTING.md)            | The pre-push gate, conventions, releasing                 |
| [AGENTS.md](../AGENTS.md)                     | Working conventions and current state                     |
| [Integration testing](integration-testing.md) | Running Axon against a local Synapse                      |
| [Packaging](../packaging/README.md)           | `.deb`, `.rpm`, Homebrew and winget                       |
| [Client parity](client-parity.md)             | Which API capabilities each client exposes                |
| [Demo coverage](demo-coverage.md)             | Which demo scene shows which capability                   |
| [Brand assets](brand/README.md)               | Logos and usage                                           |

## Why it works this way

| Document                                         | What it covers                       |
| ------------------------------------------------ | ------------------------------------ |
| [Product requirements](mvp/prd.md)               | What we are building and why         |
| [Tech spec](mvp/tech-spec.md)                    | Architecture decisions and tradeoffs |
| [Implementation plan](mvp/implementation.md)     | Milestone-by-milestone build plan    |
| [ADRs](adr/)                                     | Decisions made during implementation |
| [RFC 0001](rfc/0001-postgres-vs-sqlite-store.md) | Postgres versus SQLite for the store |

## Operator and platform notes

- [Custom emoji and sticker packs](custom-emoji.md)
- [Matrix OAuth browser sign-in](matrix-oauth-browser-sign-in.md)
- [Apple sign-in: browser](apple-oauth-browser.md), [native](apple-oauth-native.md), and the [App Review response](apple-oauth-review-response.md)
- [Web slow-link measurement](web-slow-link-measurement.md)
- [Incoming mobile sharing design](mobile-incoming-sharing.md) — iOS-first rich links, multi-file sharing and Android share-target plan
- [Web client review, 2026-07](reviews/2026-07-web-client-review.md)
- [Smoke-testing plan](mvp/smoke-testing-plan.md)
