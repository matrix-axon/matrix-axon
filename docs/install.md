# Installing Axon

Axon has two halves: a **server** that holds your Matrix state, and **clients** that talk to it.
Install a server first, then a client, then point the client at the server.

Not sure where to start?
The [Docker stack](../deploy/README.md#quick-start-prebuilt-images-no-clone) installs the server and the web client in one command on any operating system.
Everything else on this page is for a more native install.

All downloads are on the [latest release page](https://github.com/matrix-axon/matrix-axon/releases/latest).

## Server

### Docker (any operating system)

Docker Compose brings up Postgres, Axon, and the web client behind one front door.
See the [Docker guide](../deploy/README.md) for the quick start, remote access with Tailscale or Caddy, backups, and upgrades.

### macOS (Homebrew)

```sh
brew install matrix-axon/tap/axon-server
```

### Debian and Ubuntu

Debian 12 or Ubuntu 22.04 and newer, amd64 or arm64:

```sh
sudo apt install postgresql
curl -fLO "https://github.com/matrix-axon/matrix-axon/releases/latest/download/axon-server_$(dpkg --print-architecture).deb"
sudo apt install "./axon-server_$(dpkg --print-architecture).deb"
```

### Fedora and RHEL

Fedora or RHEL 10, x86_64 or aarch64:

```sh
sudo dnf install postgresql-server postgresql-contrib gawk
sudo postgresql-setup --initdb
sudo systemctl enable --now postgresql
curl -fLO "https://github.com/matrix-axon/matrix-axon/releases/latest/download/axon-server.$(uname -m).rpm"
sudo dnf install "./axon-server.$(uname -m).rpm"
```

Neither package creates or starts a database when PostgreSQL is installed, so the lines before the package do that.
Skip `postgresql-setup --initdb` on a machine that already runs PostgreSQL.

On any of these systems, the install sets up the database and the service, then prints a one-time sign-in URL for the web client at `http://127.0.0.1:8080/`.
It listens on this machine only; the same output says how to reach it from other devices and how to turn the web client off.
`/usr/share/doc/axon-server/README.Debian` has the details, and [`packaging/README.md`](../packaging/README.md) describes what the packages do.

### Windows

Download `axon-server-windows.zip` from the release page.
It is the server with the web client built in.
The [Docker stack](../deploy/README.md) is the simpler way to run the server on Windows, because it also provides the PostgreSQL database.

### From source

See [building.md](building.md).

## Clients

### Web client

The server serves the web client at its own address (`http://127.0.0.1:8080/` for a native install, the same port for Docker), so there is nothing else to install.
Open the one-time sign-in URL the install printed.
To reach it from other devices, see [TLS and remote access](server-admin.md#tls) and the [Docker guide](../deploy/README.md#tls--remote-access-profiles).

### Desktop app

The same web client packaged as a native app: an OS-level window that remembers which server you use.

| Platform                      | File                            |
| ----------------------------- | ------------------------------- |
| macOS (Intel + Apple Silicon) | `Axon_<version>_universal.dmg`  |
| Windows (x64)                 | `Axon_<version>_x64-setup.exe`  |
| Debian / Ubuntu (x64)         | `Axon_<version>_amd64.deb`      |
| Other Linux (x64)             | `Axon_<version>_amd64.AppImage` |

On macOS you can instead use Homebrew:

```sh
brew install --cask matrix-axon/tap/axon
```

On first launch the app asks for your Axon server's address: `http://<host>:8080` for the Docker stack, or your Tailscale or TLS hostname.
Then sign in with a token — `docker compose exec axon-server axon-server token issue --label desktop` — or through SSO, if your server has registered the desktop client (see [SSO sign-in](../clients/web/README.md#sso-sign-in) for the `[[oauth.clients]]` entry it needs; the Docker stack does not create one for you).
The one-time `/bootstrap/<code>` URL signs in the browser client, not the app, and minting a token consumes it — so do the browser sign-in first if you want both.
Each release also carries `THIRDPARTY-axon-desktop.txt`, the full license disclosure for everything the app bundles.

The macOS build is a universal binary, so Apple Silicon and Intel both run natively; the Windows and Linux builds are x64 only, so on ARM use the browser client or build the shell yourself ([#455](https://github.com/matrix-axon/matrix-axon/issues/455)).
The macOS build is signed with a Developer ID certificate and notarized by Apple, so it opens without a Gatekeeper warning.
The Windows executables and installer are signed through Azure Artifact Signing (ADR 0119), so they show a verified publisher; SmartScreen may still warn on the first downloads of a new release while its reputation builds.
The Linux builds are not signed, so some Linux desktops will warn the first time you run them.
Building the shell yourself is documented in [`clients/web/src-tauri/README.md`](../clients/web/src-tauri/README.md).

### Terminal client (axon-tui)

On macOS:

```sh
brew install matrix-axon/tap/axon-tui
```

On other systems, download `axon-tui` for your platform from the release page.

Point it at a running server.
For the Docker stack, no repo is needed, since the token comes from the container's own CLI:

```sh
# 1. Mint a token from the running stack:
docker compose exec axon-server axon-server token issue --label tui

# 2. Run the TUI against the front door, pasting that token:
axon-tui --base-url http://127.0.0.1:8080 --token <token>
```

The TUI reaches the API through the same `web` front door as the browser (use your `AXON_PORT` if you changed it).
For a native install, mint the token with `axon-server token issue --label tui` instead (see [Authentication](server-admin.md#authentication)).
The flags are the quickest path;
`axon-tui` also reads `AXON_BASE_URL` / `AXON_TOKEN`, or a `~/.config/axon-tui/config.toml` with a `[server]` block (`base_url` / `bearer_token`).
Minting a token creates a credential, which consumes the one-time web bootstrap if you haven't used it yet — so do the browser sign-in first if you want both the web client and the TUI.
See the [TUI guide](../clients/tui/README.md) for everything else.

### iOS and iPadOS

Install an older build [directly from the App Store](https://apps.apple.com/us/app/matrix-axon/id6813675186), or get a recent TestFlight beta build by posting in [#axon-developer:bostoncoop.net](https://matrix.to/#/%23axon-developer%3Abostoncoop.net).

### Android

Download the [Android build](https://github.com/matrix-axon/matrix-axon/releases/latest/download/app-universal-release-signed.apk).
A Play Store beta build is available; post in [#axon-developer:bostoncoop.net](https://matrix.to/#/%23axon-developer%3Abostoncoop.net) to be added to the beta distribution. Once we have sufficient beta testers, we'll be able to push a release version.
