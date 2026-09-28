# Homebrew tap for Axon

```sh
brew install matrix-axon/tap/axon-server
brew install matrix-axon/tap/axon-tui
brew install --cask matrix-axon/tap/axon
```

`matrix-axon/tap` is this repository (`homebrew-tap`).
Homebrew asks you to trust a third-party tap before it runs a formula or a cask.

`axon-server` is the server.
`axon-tui` is the terminal client and does not install the server.
`axon` is the desktop app and does not install the server.

All three are regenerated together when a stable version tag is published on
[matrix-axon/matrix-axon](https://github.com/matrix-axon/matrix-axon).
Install notes are printed by `brew install` and `brew info`.
