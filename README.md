# Axon

**Matrix, with a memory.**
Axon is a self-hosted personal agent for [Matrix](https://matrix.org) that sits between your homeserver(s) and your chat apps, and does the hard parts once, in one place: login, sync, end-to-end encryption, key backup, and full-history search.
Your apps stay light, and you never have to start over.

[![axon-web on a desktop window: the room list, a timeline with reactions and an edited message, and an open thread panel](docs/img/web-demo-desktop-poster.png)](https://matrix-axon.github.io/matrix-axon/demo.html#web-desktop)

## Why try it

- **Reinstall without losing anything.** Wipe a phone or laptop, sign back in, and your history, search, and read state are already there. Nothing to re-sync, no keys to re-verify, no index to rebuild.
- **Search everything you've ever said.** One full-text index across your whole history, including encrypted rooms and every account you use.
- **All your accounts, one place.** Personal and work, even on different homeservers, under a single search and a single API.
- **Pick up where you left off.** Start a message on your phone and finish the same draft in the terminal.
- **Threads, edits, and reactions that just work.** Axon resolves them on the server, so a late reaction to an old message is never dropped because a client's timeline window moved on.
- **Your data, your hardware.** Run it at home or on a small cloud instance; no one else holds your decrypted history.
- **Bring your own client.** Axon speaks an open, versioned API. We ship a keyboard-driven terminal client, a web client for desktop (Linux/MacOS/Windows) and mobile (iOS/Android), and native apps built from it.

If you've ever used an IRC bouncer such as [ZNC](https://en.wikipedia.org/wiki/ZNC), the idea will be familiar: Axon is the thing that stays connected, so your clients don't have to.

## See it

[![axon-tui rendering a seeded demo world: a room list, a photo timeline with inline terminal graphics, and a search across rooms](docs/img/tui-demo-poster.png)](https://matrix-axon.github.io/matrix-axon/demo.html#tui)

- **[Terminal client demo](https://matrix-axon.github.io/matrix-axon/demo.html#tui)**: threads, inline photos drawn as real terminal graphics, full-text search, and jump-to-date.
- **[Web client on desktop](https://matrix-axon.github.io/matrix-axon/demo.html#web-desktop)**: spaces, threads, image galleries, and scoped search.
- **[Web client on mobile](https://matrix-axon.github.io/matrix-axon/demo.html#web-mobile)**: the single-pane layout on an iPhone.

## Get Axon

You need a server (the always-on part) and a client.
The easiest way to get both on any operating system is the Docker stack:

```sh
curl -fsSL "https://raw.githubusercontent.com/matrix-axon/matrix-axon/refs/heads/main/deploy/docker-compose.beta.yml" -o docker-compose.yml
docker compose up -d
docker compose logs axon-server | grep 'bootstrap is armed'   # open the printed URL in your browser
```

For a native setup:

| Your system | Server                                                     | Client                                                                                                                               |
| ----------- | ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| **macOS**   | `brew install matrix-axon/tap/axon-server`                 | `brew install --cask matrix-axon/tap/axon` (desktop app), or `brew install matrix-axon/tap/axon-tui` (terminal)                      |
| **Windows** | [Docker stack](deploy/README.md)                           | [Desktop installer](https://github.com/matrix-axon/matrix-axon/releases/latest), or the web client in your browser                   |
| **Linux**   | `.deb` or `.rpm` package ([steps](docs/install.md#server)) | Web client in your browser, or the desktop app (`.deb`, AppImage) or terminal client                                                 |
| **iOS**     | —                                                          | [App Store](https://apps.apple.com/us/app/matrix-axon/id6813675186) or TestFlight beta                                               |
| **Android** | —                                                          | [APK](https://github.com/matrix-axon/matrix-axon/releases/latest/download/app-universal-release-signed.apk) or Play Store Beta       |

Every download is on the [latest release page](https://github.com/matrix-axon/matrix-axon/releases/latest).
[**Installing Axon**](docs/install.md) has full steps for every platform, and [building from source](docs/building.md) is there if you'd rather compile it yourself.

## Documentation

| I want to…                                              | Read                                                                                                       |
| ------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| Install Axon on my machine                              | [Installing Axon](docs/install.md)                                                                         |
| Run it with Docker, add TLS or Tailscale, back it up    | [Docker guide](deploy/README.md)                                                                           |
| Sign in, add accounts, manage tokens, enable push       | [Administering the server](docs/server-admin.md)                                                           |
| Look up a config option, environment variable, or limit | [Technical reference](docs/technical-reference.md)                                                         |
| Use the terminal client                                 | [axon-tui](clients/tui/README.md)                                                                          |
| Use or develop the web and desktop client               | [axon-web](clients/web/README.md), [desktop app](clients/web/src-tauri/README.md)                          |
| Build from source or contribute                         | [Building](docs/building.md), [CONTRIBUTING](CONTRIBUTING.md)                                              |
| Build my own client                                     | [OpenAPI spec](https://matrix-axon.github.io/matrix-axon/api.html), [client parity](docs/client-parity.md) |
| Understand why it works the way it does                 | [Product description](docs/mvp/prd.md), [tech spec](docs/mvp/tech-spec.md), [ADRs](docs/adr/)              |

The [documentation index](docs/README.md) lists everything else.

## Community and help

- Chat: [#axon-support:bostoncoop.net](https://matrix.to/#/#axon-support:bostoncoop.net) for help, [#axon-developer:bostoncoop.net](https://matrix.to/#/%23axon-developer%3Abostoncoop.net) for development.
- Questions and ideas: [GitHub Discussions](https://github.com/matrix-axon/matrix-axon/discussions). Bugs: [GitHub Issues](https://github.com/matrix-axon/matrix-axon/issues).
- More options, including email, are on the [support page](https://matrix-axon.github.io/matrix-axon/support/).

[Privacy policy](https://matrix-axon.github.io/matrix-axon/privacy/) · [Third-party components](THIRDPARTY.md) · [License](LICENSE.md) · [Changelog](CHANGELOG.md)
