# Browser sign-in

Browser sign-in adds a Matrix account by sending the user to their homeserver's own sign-in page in a browser.
It is for homeservers that hand authentication to an OAuth 2.0 authorization server, such as Matrix Authentication Service, and may offer no password login at all.
Unlike QR login it needs no second Matrix client.
The design is [ADR 0117](adr/0117-matrix-oauth-browser-sign-in.md).

This page is for client authors using the API and for operators diagnosing a failed sign-in.

## How a sign-in runs

1. The client sends `POST /v1/accounts/login/browser` with the Matrix user ID.
   Axon finds the homeserver from the user ID's server name and asks its authorization server for a link and a short code.
   An account being reactivated keeps the homeserver URL it was stored with, as it does for password login.
2. The client polls `GET /v1/accounts/login/browser/{flow_id}`.
   When the stage is `waiting_for_authorization`, the response carries `verification_uri` and `user_code`.
3. The user opens the link in any browser, on any device, signs in, and approves.
   The browser never needs to reach Axon.
4. The stage becomes `finalizing`, then `done` with the new `account_id`.

`DELETE /v1/accounts/login/browser/{flow_id}` cancels a flow.

A flow lasts thirty minutes at most, and less if the authorization server sets a shorter expiry.
Only one sign-in flow of any kind, browser or QR, can be open for a Matrix user at a time.

## What clients must handle

- **Always show the user code.**
  Some authorization servers put the code in the link, so the user only confirms it.
  Others return a bare link and the user must type the code.
  Axon cannot tell the client which in advance.
- **Show the link's host.**
  The link leads to the authorization server, which is often on a different host from the homeserver.
- **Present the link as something to open, not to share.**
  Anyone who holds it and can sign in as the same Matrix user can approve the sign-in.
- **The link and code disappear** once the flow leaves `waiting_for_authorization`.
  Do not cache them.
- **The account is not verified.**
  A completed sign-in is like a password login: the account is active and `verified` is `false`.
  Lead the user straight on to recovery-key import or interactive verification, or encrypted history will not decrypt.
- **Treat an unknown `error_code` as a generic failure.**
  The set may grow.

## Failure codes

A failed flow has stage `failed` and one of these in `error_code`.

| Code                    | What happened                                                                                                                         | What to tell the user                                                              |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| `unsupported`           | The homeserver has no OAuth authorization server, or it does not offer the device authorization grant.                                | This homeserver does not support browser sign-in. Try a password or QR login.      |
| `authorization_denied`  | The user declined at the authorization server.                                                                                        | Sign-in was declined. Start again if that was a mistake.                           |
| `authorization_expired` | Nobody approved before the code or the flow expired.                                                                                  | The sign-in link expired. Start again.                                             |
| `user_mismatch`         | The browser was signed in as a different Matrix user from the one entered.                                                            | You approved as a different account. Sign out in the browser, or fix the user ID.  |
| `conflict`              | The account became active, or began to be deleted, while the user was approving.                                                      | This account was signed in another way. Nothing more is needed.                    |
| `timeout`               | The authorization server or homeserver did not answer one request in time.                                                            | The homeserver is not responding. Try again shortly.                               |
| `upstream`              | The homeserver could not be found, or it or its authorization server answered in a way Axon could not use. The server log says which. | Sign-in failed at the homeserver. Try again, and contact the operator if it lasts. |
| `internal`              | A local failure: storage, configuration, or the SDK.                                                                                  | Sign-in failed on this Axon server. Contact its operator.                          |

Creating a flow can also be refused outright:

| Status | Meaning                                                                                                               |
| ------ | --------------------------------------------------------------------------------------------------------------------- |
| `400`  | The user ID is not a valid Matrix user ID.                                                                            |
| `409`  | The account is already signed in, is being deleted or is still shutting down, or another sign-in flow is open for it. |
| `429`  | Too many sign-in flows are open or recently finished. Retry shortly.                                                  |

A flow ID from QR login is `404` on these routes, and a browser flow ID is `404` on the QR routes.

## Diagnosing a failed sign-in

Every line a browser sign-in logs sits in a span named `matrix_oauth_browser_sign_in` that carries the `flow_id` and the `expected_user_id`, so searching the log for the user's Matrix ID finds the whole attempt.
The default log level is enough.
`RUST_LOG=axon_sync=debug` adds the homeserver and authorization-server hosts that discovery found, and each request to slow polling down.

A successful sign-in logs, in order:

1. `Matrix OAuth login flow created`
2. `Matrix OAuth browser sign-in is waiting for the user to approve`, with the link's host, whether the link carries the code, the polling interval, and the time allowed
3. `Matrix OAuth device authorization approved`, with the number of polls and how long the user took
4. `Matrix OAuth login finalized and supervised`, with `verified=false`
5. `Matrix OAuth login flow completed`

A failure logs a warning with a `phase` and an `error_class`, then `Matrix OAuth login flow failed` with the `error_code` the client saw.
The last line reached tells you where it stopped:

| Symptom                                                         | Look for                                                                                                                                                                                                                                                                                                                                                         |
| --------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Fails at once with `upstream`, no "waiting" line                | `phase="homeserver_discovery"`: the `error` field names the server and the step that refused. Otherwise `phase="oauth_discovery"`, `"oauth_registration"` or `"device_authorization"`.                                                                                                                                                                           |
| Fails at once with `unsupported`                                | `reason="no_endpoint"` or `reason="grant_type_not_listed"`: the authorization server does not offer the device grant. `phase="oauth_discovery"` with `error_class="unsupported"` means there is no authorization server at all.                                                                                                                                  |
| `Matrix OAuth device authorization was refused`                 | `oauth_error`. `invalid_scope` means the server rejects the scopes Axon asks for. `unauthorized_client` means the registered client may not use the device grant.                                                                                                                                                                                                |
| User says they approved, but the flow ends as expired           | The "waiting" line shows the time allowed, and the expiry line shows `polls` and `waited_secs`. No "approved" line means the server never reported approval, usually because the user approved a different code or session.                                                                                                                                      |
| "Approved" is logged, then the flow fails                       | `phase="e2ee_initialization"` with `timeout`: the homeserver did not answer the encryption setup requests that follow a new session within a minute. `phase="identity_validation"` for a wrong user, `phase="device_validation"` for a homeserver that did not bind the token to the requested device, `phase="session_validation"` for a missing refresh token. |
| User was asked to type a code they could not find               | `link_carries_code=false` on the "waiting" line: the server sent a bare link, and the client must show the code.                                                                                                                                                                                                                                                 |
| Flow stays in `waiting_for_authorization` with repeated retries | `Matrix OAuth token poll did not complete; will retry` or `token endpoint is unavailable`: the authorization server is unreachable. Five in a row end the flow as `upstream`.                                                                                                                                                                                    |

The log never contains the link, the user code, the device code, or any token.
`oauth_error` is only ever one of the standard OAuth error codes, `other`, or `none`; text chosen by the remote server is not logged.

## Configuration

Browser sign-in has no settings of its own.
It uses the Matrix OAuth client settings under `[sync.matrix_oauth]`:

- `request_timeout_secs` bounds each request Axon sends to the authorization server, including the device authorization request and every token poll.
- `static_registrations` supplies a client ID for an authorization server that does not allow dynamic registration.
  That client must be allowed the device authorization grant.

For it to work, the homeserver must advertise an authorization server whose metadata lists a `device_authorization_endpoint` and the `urn:ietf:params:oauth:grant-type:device_code` grant type.
In Matrix Authentication Service that is the `oauth.device_code_grant_enabled` setting.
