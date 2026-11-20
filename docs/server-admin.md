# Administering the server

Day-two operation of an Axon server: signing in, adding accounts, minting and revoking credentials, push, and exposing it safely.
For installing it, see [install.md](install.md) or the [Docker guide](../deploy/README.md);
for every config key and environment variable, see the [technical reference](technical-reference.md).

## Authentication

All `/v1/` API endpoints require a bearer token.
Mint one after startup:

```bash
axon-server token issue --label my-client   # prints the raw token once
axon-server token list                       # list tokens (never shows secrets)
axon-server token revoke <id>                # revoke a token by id
axon-server token revoke --label my-client   # or by label, if it uniquely identifies one active token
```

Tokens are instance-scoped — one token grants access to all accounts on that Axon instance.
Revoking a token that belongs to a sign-in session (SSO) also signs out every session that client holds for the same identity, since the session could otherwise renew itself.
Supply the token to clients via their config file or environment;
see [`clients/tui/README.md`](../clients/tui/README.md) for the TUI.

**OAuth / SSO sign-in (Google, Microsoft).**
Axon can also act as its own minimal OAuth 2.0 authorization server and OIDC relying party (ADR 0054), so the web client can sign in via SSO instead of pasting a bearer token.
This is separate from — and does not replace — the CLI token path above;
both mint the same kind of bearer token underneath.
Configure a provider (`oauth.providers.google` / `.microsoft` in `axon.toml`), then bind the owner's identity once from the command line:

```bash
axon-server oauth bind --provider google      # or --provider microsoft
axon-server oauth identities list
axon-server oauth identities unbind <id>       # atomically invalidates the identity's tokens and authorization codes
```

`bind` prints a URL — open it in any browser, on this machine or elsewhere, since it only needs to reach Axon's already-running `/v1/` surface — and polls until that browser leg completes or the 10-minute handshake expires.
Apple browser sign-in is implemented for deployments with their own registered Apple credentials; owner binding, web login, repeat login, and Google/Microsoft regression checks have been verified on a registered deployment.
Configure a Services ID (`client_id`), `team_id`, `key_id`, and exactly one of inline `private_key` or `private_key_path` under `[oauth.providers.apple]`, then set `enabled = true`.
Key files must be regular files of at most 16 KiB, with owner-only permissions on Unix (`chmod 600`); relative paths are resolved from the server/CLI working directory.
The callback is `oauth.external_base_url` (without trailing slashes) plus `/v1/oauth/apple/callback` and must be a canonical HTTPS URL without userinfo, query, or fragment.
Register that exact return URL with Apple; if `redirect_uri` is also configured, it must match exactly.
Use `axon-server oauth bind --provider apple` against the same configuration as the running server, or the explicitly armed first-run web bootstrap, to bind the owner before ordinary sign-in.
Cancellation ends the current attempt; restart sign-in or rerun the binding command to retry.
The iOS app signs in with Apple natively, which is a separate switch: `native_enabled = true` with `native_audiences = ["org.matrixaxon.axon"]` under `[oauth.providers.apple]`, needing no Services ID or key; no project signing key is distributed to self-hosters.
`enabled` is the browser flow only; for iOS-only Apple sign-in leave it `false`, since `enabled = true` without the browser credentials above stops startup.
The [native server contract](apple-oauth-native.md) provides single-use challenges, owner-authorized binding, and atomic redemption; link an Apple ID from the app's Settings after signing in some other way.
See [ADR 0054's implementation addendum](adr/0054-oauth-authorization-server.md#implementation-addendum-apple-rollout) for the remaining server and mobile work.
The [Apple browser verification guide](apple-oauth-browser.md) covers configuration, automated checks, and the required registered-deployment acceptance pass.

**Remote push.**
A suspended iOS, iPadOS, or Android client registers a device token with `PUT /v1/push/registrations` and removes it with `DELETE /v1/push/registrations/{id}`.
The packaged iOS, iPadOS, and Android app registers when message notifications are on, and deletes that registration when they are turned off or the Axon session ends.
`GET /v1/status` reports `push.apns` and `push.fcm`, true when this process loaded that provider's key.
Registration succeeds when the key is not configured.
Waking a device is a separate step from storing the registration.
Reading a room on any device also reaches suspended phones (ADR 0129).
An iPhone or iPad whose registration has `badge` set gets the instance unread total on every alert, and a badge-only push when a count falls.
An Android device gets a silent message that withdraws the room's notification once that room is fully read.
These sends use the same keys, and need no extra configuration.
Set `[push.apns]` with `team_id`, `key_id`, `bundle_id`, and exactly one of `private_key` or `private_key_path`.
Set `[push.fcm]` with `project_id`, `client_email`, `package_name`, and exactly one of those key sources.
A provider whose every field is empty stays off.
A provider that is only partly set fails while the configuration is loading, before Axon contacts the database.
A key file that cannot be read, or a private key that is not that provider's key type, also stops startup.
Key files are regular files of at most 16 KiB, owner-only on Unix, same as the Apple sign-in key, and that sign-in key stays under `[oauth.providers.apple]`.
The environment names are `AXON_PUSH__APNS__PRIVATE_KEY`, `AXON_PUSH__APNS__PRIVATE_KEY_PATH`, `AXON_PUSH__FCM__PRIVATE_KEY`, and the same shape for the other fields.

The Matrix OAuth session foundation for Axon's own homeserver sessions is configured separately under `sync.matrix_oauth` (ADR 0097).
It dynamically registers a public client when the discovered authorization server permits it; operators can configure a static public client ID for issuers that disable dynamic registration.
Access and refresh tokens are encrypted with `sync.store_key`, and client secrets are not supported.
The authenticated QR acquisition API is available at `POST /v1/accounts/login/qr` and its flow-specific polling/input routes; it supports either displaying QR data or submitting a decoded scan and activates the account only after Axon independently confirms that its new device is cross-signed.
Browser sign-in (ADR 0117) is a third way to add an account on such a homeserver, for a user with no second Matrix client to scan with.
`POST /v1/accounts/login/browser` starts a flow for a Matrix user ID; the flow yields a link and a short code, the user opens the link in any browser on any device and approves, and the client polls `GET /v1/accounts/login/browser/{flow_id}` until it is `done`.
The link is valid for up to thirty minutes and is returned only while the flow waits for approval.
It needs a homeserver whose authorization server offers the OAuth device authorization grant, and reports `unsupported` otherwise.
An account added this way is signed in but its device is not verified; verify it afterwards with a recovery key or interactive verification.
The web client offers it on the Accounts page as **Sign in with browser**; the TUI does not yet.
[Browser sign-in](matrix-oauth-browser-sign-in.md) lists the failure codes and how to diagnose a failed sign-in from the server log.
The web client exposes both QR presentations on its unified `/accounts` page, including camera or image scanning, check-code handling, authorization approval, cancellation, and reload resume.
The TUI presentation remains a later client-specific slice.
The existing password and token-import account routes remain available.

For first launch only, an interactive server can mint the first credential from the one-time `/bootstrap/<code>` URL printed at startup instead of requiring `axon token issue` in another shell.
The bootstrap page is loopback-only by default.
To allow a trusted remote browser during setup, set `server.bootstrap_web_allow_remote = true` (or `AXON_SERVER__BOOTSTRAP_WEB_ALLOW_REMOTE=true`) and make sure the server is fronted by TLS, a proxy, or a trusted network.
If `server.web_client_url` is set, the bootstrap success page links to that web client after showing the token;
the token is never placed in the URL.
A server that serves the web client itself (below) uses `/` when that key is unset.
Set the key when the web client is not at `/` of the address the operator's browser uses, for example behind a reverse proxy that publishes the server under a path prefix.

The bootstrap is offered only on an interactive TTY by default (the operator answers a prompt).
Headless or containerized deployments have no TTY, so they can arm it non-interactively with `server.bootstrap_web_auto = true` (`AXON_SERVER__BOOTSTRAP_WEB_AUTO=true`);
it still only arms when no credential exists yet, and the loopback / `bootstrap_web_allow_remote` gate is unchanged.
When `server.web_client_url` resolves to the **same origin** as the bootstrap page (e.g. a reverse proxy that serves the web client and proxies `/bootstrap` on one host), the bearer success page shows the freshly minted token with a "Copy token" button, and its "Open web client" button writes the token into the web client's `localStorage` and goes there — signing the operator in with nothing to paste, and still without the token ever appearing in a URL.
The page waits for that click because it is the only time the token is shown.
(This same-origin hand-off is bearer-token only; the SSO flow shows its tokens to copy.)

A binary built with the `embed-web` cargo feature carries the web client and serves it at every path that is not the API, on the same address and port (ADR 0116).
Build one with `pnpm --dir clients/web install && pnpm --dir clients/web build`, then `cargo build --release -p axon-server --features embed-web`;
Set `AXON_WEB_DIST` to the bundle's directory, as an absolute path, to make a missing bundle a build error: without it, an unbuilt `clients/web/dist` is only a cargo warning and the binary carries no web client.
The server's startup log says which it is.
The web client is reachable exactly where the API is, which is this machine only unless `server.host` says otherwise.
To serve the API alone, set `server.web_client = false` (or `AXON_SERVER__WEB_CLIENT=false`).
A binary built without the feature answers those paths with a page saying no web interface is served.

With `bootstrap_web_allow_remote = true`, any client that can reach the bootstrap surface — including one just probing it — shares the same six-wrong-URL lockout as the operator: six bad requests to `/bootstrap`, `/bootstrap/token`, `/bootstrap/oauth/{provider}`, or a wrong `/bootstrap/{code}` permanently close the bootstrap surface for the rest of the process, forcing a restart to try again.
This is an availability risk on top of the confidentiality one, so treat remote bootstrap as no safer than any other unauthenticated surface exposed off loopback.

To explicitly retry stored Unable-To-Decrypt events for an active account, call the authenticated API through the CLI wrapper:

```bash
AXON_TOKEN=<token> AXON_BASE_URL=<axon-server-url> axon utd redecrypt --account-id <account-id>
```

`AXON_BASE_URL` defaults to `http://127.0.0.1:8080` if unset — set it explicitly whenever the server isn't on localhost, or the request silently goes nowhere useful and fails with a 401 (the token is fine; it's just being checked against the wrong server).
See [Environment variables](technical-reference.md#environment-variables) for the full list of vars axon reads.

## Changes to an account's Matrix identity

[ADR 0136](adr/0136-exclude-insecure-devices-msc4153.md) adds `own_identity_changed` to `GET /v1/accounts` and `GET /v1/accounts/{account_id}`.
The first observed master key establishes a baseline.
On upgrade, Axon can also report an earlier replacement when a consistent SDK read contains both the master key and the retained verification violation for the account's own identity.
Detection of a pre-baseline replacement depends on that retained evidence being available at the first complete observation.
The warning is true when the key has been replaced and Axon's device still needs to be cross-signed by the replacement identity.
Use the existing recovery-key or interactive verification flow to re-verify Axon.
The server retains a pending warning across restarts and logout, and clears it when it observes that the device is cross-signed by the current identity.
Recovery normally clears the warning in its response.
If the SDK's identity is temporarily unavailable, the watcher clears it once it can confirm the signature.
Verified QR sign-in clears the warning on activation.
If the account's key changed while it was signed out, the watcher can subsequently announce the replacement with the warning already clear.

On `/v1/ws`, `account.own_identity_changed` carries the account ID in the envelope and two booleans in the payload.
`own_identity_changed` is the current warning state.
`identity_replaced` is true when this update observed a replacement key, including when the device is already cross-signed by that identity.
Clients can re-read the account on reconnect to recover its pending warning.

## Managing the server from a client

A signed-in client can administer the server through the management API under `/v1/management/` (ADR 0109), so the common jobs do not need a shell on the machine.
Today that is listing the sign-in identities linked to you, linking another and unlinking one, and listing, minting and revoking the tokens clients sign in with;
search-index rebuilds and the decryption backlog follow.
A minted token is shown once, in the response that creates it, exactly as `axon-server token issue` prints it once.

Changing credentials asks for more than being signed in.
The request must come from a token that never expires (one minted by `axon-server token issue`, `init` or the management API itself), or from a session that signed in with its provider in the last ten minutes;
otherwise the server answers `recent_sign_in_required` and the client signs you in again.
Removing the last way to sign in is refused until you confirm it.

It is on by default.
To keep these operations behind the command line, set `server.management_api = false` (or `AXON_SERVER__MANAGEMENT_API=false`): every management route then answers `403`, and the `axon-server` subcommands keep working.

## TLS

Axon serves plain HTTP.
For any non-local deployment, place a TLS-terminating reverse proxy (Caddy, nginx, etc.) in front of it and keep Axon bound to loopback (the default).
Axon refuses to start on a non-loopback address over plain HTTP unless `AXON_SERVER__ALLOW_INSECURE_BIND=true` is explicitly set.
The `caddy` profile in the full `deploy/` stack automates this — see [deploy/README.md](../deploy/README.md).
A proxy that keeps an access log must keep video and audio stream tickets out of it; see [Stream tickets](technical-reference.md#stream-tickets).

## Network tuning

A Linux server's default TCP congestion control, cubic, can slow some clients enough that video buffers and large downloads crawl.
It happens on paths that are long or lose a packet now and then, as home Wi-Fi often does.
A player's requests share one connection, and on such a path cubic can hold that connection to a small fraction of the link.
On one deployment, a client on a 50 Mbps home link received about 0.5 MB/s from a server 60 ms away, which is too slow for a 720p video.
A speed test does not show this, because it uses many connections to a nearby server.
BBR does not slow down for occasional loss. With it, the same client received 5 to 7 MB/s.
Other paths gain less, and one that does not lose packets gains nothing.

The setting applies to the TCP connections between clients and whatever accepts them: the reverse proxy, or with the Docker stack, the `web` or `caddy` container.
That covers HTTP/1.1 and HTTP/2.
HTTP/3 runs over QUIC, which has its own congestion control in the proxy, so clients that negotiate it are unaffected either way.
Axon's connection to the proxy is local and gains nothing; its connection to a distant homeserver would gain, and keeps the host's setting unless the host uses BBR.

**Docker stack.**
The `web` and `caddy` containers already use BBR for the connections that end in them, through a per-container `sysctls` setting, and the kernel loads its BBR module when they start.
If either container fails to start with `failed to write sysctl net.ipv4.tcp_congestion_control`, the container could not use BBR, for one of these reasons:

- The host kernel has no BBR. It arrived in Linux 4.9, and per-container settings in 4.15; some NAS kernels are older or leave it out.
- The runtime cannot load the module. Rootless Docker and Podman cannot, so BBR works there only if the host has already loaded `tcp_bbr`.

Set `AXON_TCP_CONGESTION_CONTROL=cubic` in `.env` to start without it.
A runtime that refuses container `sysctls` altogether needs them removed instead: in `docker-compose.override.yml`, give `web` and `caddy` each `sysctls: !reset {}` (Compose 2.24 or later).

Some connections reach the published port without ending in the container, and they keep the host's setting:

- **A reverse proxy on the host** in front of the stack.
- **Docker's userland proxy**, which accepts connections on the host when the kernel cannot forward them, such as IPv6 clients when the container network has no IPv6.
- **Rootless Docker and Podman**, whose port forwarders accept every client connection on the host.

For those, set BBR on the host as below as well.

**Any other Linux server** (packages, a reverse proxy on the host):

```bash
sudo tee /etc/sysctl.d/90-axon-bbr.conf <<'CONF'
net.core.default_qdisc = fq
net.ipv4.tcp_congestion_control = bbr
CONF
sudo sysctl --system
sysctl net.ipv4.tcp_congestion_control   # bbr
```

A file in `/etc/sysctl.d/` that sorts later, or `/etc/sysctl.conf`, overrides this one, so check that none sets either key back.
New connections use BBR at once, a reverse proxy's included, without a restart.
A container keeps the default its network had when it was created, so recreate any that accept client connections (`docker compose up -d --force-recreate`).
The interface's queueing discipline changes to `fq` at the next reboot; BBR works without it.

The setting does not reach three set-ups:

- **Tailscale's userspace networking**, which the stack's `tailscale` profile uses: the sidecar's own TCP stack carries client connections, not the kernel's.
- **Docker Desktop** on macOS or Windows: client connections end at Docker Desktop's port forwarder in the desktop OS, not in its Linux VM.
  The containers' setting applies inside the VM, whose kernel decides whether they start with it; if they do not, set `AXON_TCP_CONGESTION_CONTROL=cubic`.
- **A macOS server**, which has no BBR.
