# ADR 0109 — A management API, so a client can administer the server

**Status:** Accepted.
Implemented in steps, tracked in #587; see "Sequence" below.
Step 2 (the switch, step-up, identity list and unbind) and step 4 (tokens and binds) are in.

## Context

Four operator jobs need a shell on the machine running `axon-server`:

- `axon-server token issue | list | revoke`: the bearer tokens clients use.
- `axon-server oauth bind` and `oauth identities list | unbind`: which upstream sign-in identities belong to the owner.
- `axon-server search reindex`: rebuild the full-text index.
- `axon-server utd redecrypt`: retry the undecryptable-event backlog.

A user who reaches Axon only through a client, on a Docker host, a hosted box, or a phone, can do none of these.
They cannot issue a token for a second device, revoke a lost one, rebuild a broken index, or see which sign-in identities are linked.

The gap is already visible in shipped behavior.
ADR 0054 records that "no `/v1` route lists linked identities", which is why the web client's Settings can offer "Link an Apple ID" only when the current session is not an Apple one, and can never offer "Unlink".
An App Store submission needs an in-app way to remove a linked Apple account.

What the code looks like today, which shapes the decision:

- **UTD retry is already an API call.**
  `POST /v1/accounts/{account_id}/utds/redecrypt` exists and the CLI verb is a client of it.
  What is missing is a count of the backlog.
- **Token and identity operations are thin wrappers over `Store` methods** (`issue_token`, `list_tokens`, `revoke_token`, `list_identities`, `delete_identity`, `create_bind_request`, `find_bind_request`).
  The CLI connects to the database and calls them directly.
- **`search reindex` does not rebuild anything.**
  It removes the seed-completion marker, and the rebuild happens on the next server start.
  A user without a shell cannot restart the server.
- **The auth layer discards the caller's identity.**
  `Store::verify_token` returns the token's id, but `TokenVerifier::verify` reduces it to a `bool`, so no handler can tell which token made a request.
- **"No admin API" is a written non-goal**, in `AGENTS.md`, `docs/mvp/implementation.md` ("What not to build") and `docs/mvp/prd.md` ("Out of scope for MVP").

## Decision

Axon gains a management API under `/v1/management/`, on by default, that an operator can switch off in the config file.
This supersedes the "no admin API" non-goal.
The three documents that state it are corrected in the first implementation PR, not here, so that they keep describing what the code does.

The scope is runtime state: credentials, identities, the search index, the decryption backlog.
It is not a config editor.
OAuth provider client ids and secrets, `external_base_url`, and every other key stay in the config file and the environment.

### One prefix, one gate

Every management route lives under `/v1/management/` in its own sub-router, wrapped in a single layer that enforces the config switch.
This is the same construction the bearer gate uses in `axon-api/src/lib.rs`: a layer over a sub-router rather than a check per route, so there is no route that can be added without it.
The sub-router sits inside the existing `require_bearer` layer, so a management request is authenticated before the switch is consulted.

### The switch

`[server] management_api`, default `true`, also settable as `AXON_SERVER__MANAGEMENT_API`.

When it is `false`, every `/v1/management/` route answers `403` with the error code `management_disabled`.
`GET /v1/status` gains a `management` object carrying `enabled`, so a client hides the management UI instead of probing for it.

`403` rather than the `404` that disabled OAuth routes return, because the two cases differ.
A disabled OAuth route is reached before sign-in and should not describe the instance to a stranger.
A management route is reached by an authenticated owner, who is better served by being told the operator turned it off.

### The existing UTD retry route stays where it is, ungated

`POST /v1/accounts/{account_id}/utds/redecrypt` has shipped, and both the CLI and the web client call it.
Moving it under the gate would break `axon-server utd redecrypt` on any instance that disables management, and would be a breaking API change under ADR 0099.
Only the new backlog-count read goes under the prefix.

### Who may manage: any bearer reads, credential changes need a recent sign-in

Axon has no token scopes, and one human owns the instance.
A bearer can already log Matrix accounts in, read every message, and delete an account.

Management routes that change credentials would add something worse than any of that.
A stolen one-hour OAuth access token could mint a bearer that never expires, revoke every other token, unbind every identity, and bind an identity the attacker controls.
That is takeover of the instance and lockout of its owner, and an audit trail is no help to an owner who can no longer get in.

So the routes split in two.

**Reads and index maintenance need only a valid bearer:** the token list, the identity list, bind status, the backlog count, and the search rebuild.
This is a decision, not an oversight.
The identity list shows which providers are linked and the email each reported, so a stolen short-lived bearer can read them.
That bearer can already read every message in every account, and gating the list behind step-up would stop a signed-in owner from seeing what is linked without re-authenticating first.
The provider's subject, the value that actually identifies the owner to the provider, is not returned.

**Credential changes need step-up:** minting a token, revoking a token, starting a bind, and unbinding an identity, with or without `allow_lockout`.
A request passes if the calling token is either:

- **non-expiring**, which is a token minted by the CLI, by `axon-server init`, or by this API; or
- an OAuth access token whose session began with an **interactive sign-in in the last ten minutes**.

Otherwise the route answers `403` with the code `recent_sign_in_required`.
The client runs the sign-in flow it already has (the provider redirect, or the native sheet), adopts the new session, and retries.

The sign-in time has to survive refresh, or refreshing would defeat the check.
`oauth_refresh_tokens` and `tokens` each gain a nullable `authenticated_at`.
It records when the upstream provider says the owner authenticated, and is copied unchanged through every refresh rotation.

It is never the moment Axon redeemed the identity token.
An identity token stays valid well after it is issued, so a stolen, unredeemed one would otherwise buy a fresh window on redemption.
OIDC keeps the two instants apart, `auth_time` for the authentication and `iat` for the token's issuance, and the time is read from the signed claims:

- `auth_time` when the token carries it, capped at `iat`.
- Otherwise `iat`, but only for a token bound to a nonce this server issued.
  The nonce ties the token to one sign-in started minutes earlier, so its issuance is that sign-in.
  Google emits no `auth_time`, so for it this is the only evidence available, and it cannot tell a fresh password entry from a provider session being reused.
- Otherwise nothing: a nonce-free token with no `auth_time` has unknown freshness, and unknown fails step-up.

Measured against the real providers on 2026-10-05, comparing the recorded time with the moment the session was minted:

| Provider                   | Recorded time              | What it is                                                       |
| -------------------------- | -------------------------- | ---------------------------------------------------------------- |
| Google (browser)           | 0.4 s earlier              | `iat`; no `auth_time`                                            |
| Apple (browser and native) | 1.6 to 2.0 s earlier       | within a second or two of the sign-in                            |
| Microsoft (browser)        | 5 min 0.7 s earlier, twice | `iat`, which Microsoft backdates by five minutes; no `auth_time` |

Microsoft's second reading was taken by signing in again with its own session still live, and the recorded time moved forward by the same amount as the clock, so it is a backdated issuance and not a remembered authentication.
The consequence is that a Microsoft session has about five minutes for credential changes rather than ten.
That errs toward asking again, which is the safe direction, and the window is not widened to compensate: nothing in the token distinguishes Microsoft's backdating from a token that really is five minutes old.

The browser flow verifies the token at the callback and mints at code redemption, so the verified time is kept on the authorization request in between.
A stolen access token, and a stolen refresh token, both carry the original time and cannot move it forward: only a fresh proof from the upstream provider does.
Rows that predate the column have no time and count as not recent.

A non-expiring token passes without step-up, deliberately.
It is the credential an operator creates on purpose and can revoke, it is what the CLI has always been able to do, and a client holding one has no upstream identity to re-prove.
A stolen non-expiring token is therefore full control of the instance, as it is today.
Operators who would rather no bearer had that power set `management_api = false`.

**The existing native Apple bind route takes the same rule.**
`POST /v1/oauth/apple/native/challenge` and `/token` with `purpose=bind` accept any active bearer today (ADR 0054).
Left alone, that is a way around step-up: bind the attacker's Apple ID with a stolen bearer, sign in with it, and arrive with a fresh sign-in time.
So `purpose=bind` there requires the same non-expiring-or-recent bearer.
This narrows who may call an existing route, with a status code the route already documents, and it amends ADR 0054's statement that any OAuth-issued bearer may bind.

Visibility stays, as the second line of defense:

- `tokens` gains a `created_by_token_id` column, recorded on every API mint.
- Every mint, revoke, bind and unbind writes a `tracing` line with the acting token's id and the target's id.
  No secret is logged.
  A removal that overrides the lockout guard is logged at `warn`, since it is the one path that can lock the owner out.
- The token list returns every token, including revoked ones, so an unexpected entry is visible to the owner.

To make any of this possible, `TokenVerifier::verify` returns the token's id, expiry and sign-in time, and `require_bearer` attaches them to the request as an extension that handlers can extract.
The WebSocket upgrade, which verifies the token itself, changes with it.

### Guarding against lockout, with an override

With no shell, losing the last credential is unrecoverable.
The first-run web bootstrap does not reopen: `first_credential_bootstrap_available` counts revoked and expired tokens as proof that bootstrap was already used.

So a revoke or unbind that would leave the instance with no surviving credential answers `409` with the code `last_credential`.

A surviving credential is one of exactly two things:

- an active token with no expiry; or
- a bound identity the owner could actually sign in with: OAuth is enabled, and that identity's provider is enabled for at least one flow (browser, or native for Apple).

An expiring OAuth access token is never a survivor, however long it has left.
An identity whose provider is disabled is not one either: it cannot produce a session.

The count is taken on the state the write would leave behind.
Unbinding an identity also revokes that identity's tokens, so the guard counts after that cascade, not before it.

A shared transaction is not enough to make this safe.
Under Postgres's default `READ COMMITTED`, two concurrent requests revoking token A and token B would each count the other as the survivor and both commit.
So every credential-removing write takes one transaction-scoped advisory lock (`pg_advisory_xact_lock`, the mechanism the first-credential bootstrap already uses) before it counts.
The lock is taken inside `Store::revoke_token` and `Store::delete_identity` themselves, so the CLI's revoke and unbind serialize against the API's, though only the API applies the guard.

It is a confirmation, not a prohibition.
The request may be repeated with `allow_lockout=true`, and the client's job is to explain the consequence before it does.
A hard refusal would make unlinking impossible for a user whose only credential is a linked identity, and unlinking must always be completable.

Revoking the token that made the request is otherwise allowed.
It is how a device signs itself out everywhere.

Only a token that is itself a surviving credential can be the last one, so only revoking an active non-expiring token is ever refused.
Revoking an OAuth session's access token always goes through, even on an instance with no survivor left: that token was never a way back in, and refusing would stop the owner ending a session they do not recognize.

Revoking a session's access token has to end the session, and the access token is only half of it: left alone, the refresh token would mint a replacement on the client's next request.
Nothing ties an access token to the refresh chain that minted it, only to its identity and client, so the revoke also revokes every refresh token for that identity and client.
That is the cut refresh-token reuse detection already makes (ADR 0054), and it has the same width: every session that client holds for that identity is signed out, the caller's own included if it is one of them.
Access tokens those sessions already hold are not chased; they expire within the access-token lifetime.
Ending exactly one session would need a session id carried from the refresh chain onto each access token, which this record does not add.

### A minted secret crosses the API once

The response to a mint is the one place a raw bearer token appears in an API response.
It is sent with `Cache-Control: no-store` and `Referrer-Policy: no-referrer`, as the OAuth token responses already are, and it is never logged.
This is the same sanctioned exception the secrets rule in `AGENTS.md` grants `axon-server token issue`: a value surfaced once, at the moment of issue, for the user to consume.

### Binding an identity without the CLI

`POST /v1/management/oauth/binds` creates the bind request that `axon-server oauth bind` creates today, and returns the browser URL and an id to poll.
The owner's bearer replaces shell access as the thing that authorizes the request.
The browser leg is the existing unauthenticated `GET /v1/oauth/bind`, the upstream redirect, and the existing callback, all unchanged.

The validation the CLI does before creating a request (OAuth enabled, the provider enabled and fully configured, `external_base_url` set) and the user-code generator move out of `axon-server` to where both callers can reach them.
The CLI's separate copy of the ten-minute handshake lifetime goes away with that move.

One question stays with each caller, because the two cannot answer it the same way: whether the provider is ready.
The running server asks the provider set it built at boot, which is the truth about what the browser leg can redirect to.
The CLI has no running server to ask and reads the configuration.
Everything else is one function in `axon-api`: OAuth enabled, the provider one Axon knows, the base URL set, and the order those are checked in.

A bind this server cannot start answers `409` with the code `bind_unavailable`: OAuth is off, or the provider is not enabled for browser sign-in.
Apple with only its native flow enabled is such a case, since there is no browser provider to redirect to.
A bind's status is `pending`, `completed` or `expired`; a canceled or failed sign-in reads as `expired`, and so does a pending bind past its time.
A bind that did not complete is swept once it lapses, so a later read is a `404`.
A completed one is kept for a day past its expiry, so a client that polls late (a phone that was in the background) still reads `completed` instead of a missing record it would have to report as a failure.

At most five binds may be pending at once; a sixth answers `429`.
Each pending bind is a code the unauthenticated browser leg accepts for ten minutes, so the number outstanding multiplies a guesser's odds, and the rate limiter on that leg counts requests, not open codes.
The cap applies to the CLI verb too, since both go through the same start.

### Unbinding, and what it cannot do

`DELETE /v1/management/oauth/identities/{id}` calls `Store::delete_identity`, which revokes the identity's tokens, deletes its refresh-token chain, and removes the row in one transaction.

Unbinding also revokes the non-expiring tokens that identity's sessions minted through `POST /v1/management/tokens`, and any those tokens minted in turn, followed through `created_by_token_id`.
A minted token carries no identity of its own, so otherwise a token minted from a session would outlive the unbind meant to end everything that sign-in could do: someone holding a freshly stolen session could mint one and keep it.
The cost is that a device token the owner minted while signed in with that identity stops working when they unlink it.
The lockout guard counts after this cascade, so an unbind that would leave nothing but its own descendants is still refused with `last_credential`.
The CLI's `oauth identities unbind` goes through the same store call and cascades the same way.

This is Axon forgetting the identity.
It does not revoke the upstream provider's authorization.
For Apple, the revocation call in TN3194 needs a client-secret JWT signed with the app's own key, and a token obtained by exchanging an authorization code.
The native path is keyless by design, and a self-hoster cannot hold the published app's key.
So the client's unlink confirmation points the user at the provider's own controls (for Apple, the Sign in with Apple list in the device's account settings).
The Apple revocation question that ADR 0054 leaves open stays open; this record does not close it.

### Rebuilding the search index while the server runs

`POST /v1/management/search/reindex` answers `202` and the rebuild proceeds in the indexing actor, which owns the only index writer.

The order is what keeps it crash-safe.
The actor removes the seed-completion marker first, then clears the index, reseeds from Postgres, and stamps the marker again.
A crash at any point leaves an unmarked index, which the next start already treats as "reseed from scratch".
That is the existing recovery path, reached by a new route.

Search keeps answering during the rebuild, from a partial index.
`GET /v1/status` gains a `search` object with `enabled`, a `state` of `ready` or `seeding`, and seed progress, so a client can say that results are incomplete instead of presenting them as the whole answer.

A second request while a seed is running answers `409`.
A request when `search.enabled = false` answers `409`.

The API reaches the actor through a new consumer-owned port beside `SearchQuery`, with its adapter in the `axon-server` composition root (ADR 0021), so `axon-api` still does not depend on `tantivy`.
The CLI verb keeps its offline behavior: it runs without a server, and removing the marker is all it can do.

### The decryption backlog becomes countable

`GET /v1/management/accounts/{account_id}/utds` returns how many events are pending decryption for the account, read from the existing `events_pending_utd_idx`.
With the existing retry route, a client can show the backlog, offer the retry, and show what it achieved.

### Routes

| Route                                           | Purpose                                                |
| ----------------------------------------------- | ------------------------------------------------------ |
| `GET /v1/management/tokens`                     | List tokens. No secrets. Marks the caller's own token. |
| `POST /v1/management/tokens`                    | Mint a token with a label. Returns the secret once.    |
| `DELETE /v1/management/tokens/{id}`             | Revoke a token.                                        |
| `GET /v1/management/oauth/identities`           | List bound identities.                                 |
| `DELETE /v1/management/oauth/identities/{id}`   | Unbind an identity and revoke its sessions.            |
| `POST /v1/management/oauth/binds`               | Start a bind for a provider.                           |
| `GET /v1/management/oauth/binds/{id}`           | Read a bind's status.                                  |
| `POST /v1/management/search/reindex`            | Rebuild the search index.                              |
| `GET /v1/management/accounts/{account_id}/utds` | Count the decryption backlog.                          |

All additions are new routes or new optional response fields, so the OpenAPI compatibility guard (ADR 0099) passes without an exception.

### Bounds

Each route crosses the client boundary, so each states its limits.
A token label is length-capped before it is stored, and request bodies are capped before parsing.
A label is also refused if it contains control characters or invisible format characters (zero-width, bidirectional overrides), because `axon-server token list` prints labels to a terminal, or if it starts with `oauth:`, the prefix of the label Axon generates for a sign-in session.
The first-run bootstrap page applies the same rules by cleaning the label instead of refusing it, since that page has no way to ask again.
The identity and token lists are bounded by what one human creates, and the token list is capped in the query regardless.
That cap needs an order to be safe.
Expired OAuth access tokens are never deleted, and a signed-in client leaves one behind every hour, so they come to outnumber everything else.
The list therefore returns the tokens that still work first and the dead ones after, newest first within each, and the cap of 500 only ever drops the oldest dead ones.
Bind requests reuse the existing expiry and sweep.
A rebuild is single-flight by construction, since one actor owns the writer.

## Sequence

One silo per pull request, server first.

1. This record.
2. Server: the switch, the authenticated-token extension with the sign-in time and the step-up check, identity list and unbind.
   This step also corrects the "no admin API" non-goal in `AGENTS.md`, `docs/mvp/implementation.md` and `docs/mvp/prd.md`.
3. Web: linked sign-ins in Settings, with unlink.
   Steps 2 and 3 go first because they are what an App Store submission needs.
4. Server: tokens and binds.
5. Server: live reindex, the `search` status block, and the backlog count.
6. Web: Settings reorganized into groups, with no new behavior.
7. Web: the management panels, placed in that structure.
8. TUI: either the same operations as commands, or a recorded decision that it stays a web capability.

`docs/client-parity.md` gains a row in each step that changes a client's status.

## Consequences

- An instance can be administered end to end from a client.
  The CLI remains, unchanged, as the path that needs no running server and no existing credential.
- The first token still has to come from somewhere outside this API: `axon-server init`, `token issue`, or the first-run web bootstrap.
  Management requires a bearer, so it cannot create the first one.
- A leaked non-expiring bearer is worth more than before: it can create and remove credentials.
  A leaked OAuth bearer is not, because it cannot produce a fresh sign-in.
  Operators who would rather keep that power behind a shell set `management_api = false`.
- A user signed in through a provider is asked to sign in again before changing credentials, if their last interactive sign-in is more than ten minutes old.
- Linking an Apple ID from a provider session now asks for that same fresh sign-in, where before any active session could link.
- An operator who disables management also disables unlinking from a client.
  On a single-owner instance the operator and the user are the same person, and the CLI verb remains.
- Clients take on a new display duty: a token secret shown once and never recoverable, and a search result set that may be partial during a rebuild.

## Alternatives considered

**Gate each handler instead of a sub-router.**
Rejected for the reason the bearer gate is a layer: a per-route check is one a new route can forget.

**Hide disabled routes behind `404`.**
Rejected above; the caller is the authenticated owner.

**Refuse outright to remove the last credential.**
Rejected because unlinking must be possible for a user whose only credential is the identity being unlinked.

**Allow any valid bearer to change credentials, and rely on the audit trail.**
This was the first draft.
Rejected in review: a stolen short-lived bearer could take the instance over and lock the owner out, at which point the trail is unreadable to the one person it is for.

**Require a non-expiring bearer for credential changes.**
Simpler than step-up, and it closes the same hole.
Rejected because the user this feature is for often has no such token: someone signed in on a phone with Apple could not unlink Apple from the app.

**Tokens flagged as administrators.**
This is a scope model.
It may well be right, but it changes what every existing token means, and it deserves its own record.

**Expose `reindex` as "marker removed, restart to rebuild".**
It would reuse the CLI's code path exactly, and it would be useless to the user this feature is for, who cannot restart the server.

**Let the API edit the config file.**
Out of scope.
The config file carries secrets and the precedence rules in ADR 0051, and a route that writes it would have to reimplement both.

## Related work, decided separately

Native "Sign in with Google" on mobile, verified without a browser callback in the way native Apple sign-in is, builds on the bind and unbind routes here and on nothing else in this record.
It needs a per-provider generalization of the native challenge endpoints, a Google SDK in the shell's native-auth plugin on each platform, and publisher-side registrations.
It gets its own record.
