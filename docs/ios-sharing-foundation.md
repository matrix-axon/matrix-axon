# iOS incoming-share foundation

The iOS app has a durable pending-share inbox at **Pending shares** (`/pending-shares`).
Incoming share-sheet ingestion and the native picker/composer remain subsequent stages of [ADR 0125](adr/0125-ios-first-incoming-sharing.md).
Distribution builds do not register an unfinished share target.

## Recovering a draft

Open Pending shares and choose **Review share**.
Confirm the sending Matrix account and joined room, edit the text or first attachment caption, and review previews and saved delivery outcomes.
An ordinary room draft stays separate.
Typing is saved after 250 milliseconds of idle time, with a one-second maximum batching window; leaving the editor, hiding the page, and Send flush pending text.
A sudden process termination can lose text still inside that short batching window.
After ownership expires, resume or Refresh reclaims an available share and preserves unsaved editor text without sending it.
**Keep draft** retains the incoming share; it does not send or acknowledge delivery.
Unfinished records expire after 24 hours, and **Discard share** explicitly removes local content.

**Include preview** asks the selected account's homeserver to look up the URL and therefore discloses that URL to the homeserver.
Neither opening recovery nor rendering a card fetches the linked page.
Generated and source thumbnails are retained locally, independently of server upload expiry.
Sending re-uploads retained thumbnails without regenerating the webpage.
A missing or corrupt thumbnail requires explicit replacement or image removal before sending.
Changing the matched URL removes its old card.

Send validates the destination against current rooms and preserves oversized drafts for editing.
The import ceiling is 64 KiB of UTF-8 text; it is not a send allowance.
Preview fields use the server's UTF-16 limits, with a separate 32 KiB canonical preview budget and 40 KiB content budget including protocol aliases and media/encryption reservations.
Storage is bounded to 20 shares/200 MiB, with ten user attachments and 100 MiB per file/batch; thumbnail bytes count toward storage without becoming separate messages.
The total includes conservative reservations for metadata and atomic replacement: 2 MiB per record plus 16 KiB for session control files.
Thus a nearly full inbox can reject a batch below its independent 100 MiB ceiling; existing drafts are never evicted to admit it.

Confirmed items retain their event IDs and are never resent during recovery.
An interrupted dispatch is **outcome unknown**, because the server may have accepted it without returning a response.
Inspect the destination before explicitly allowing a resend; the duplicate-risk checkbox never triggers a send itself.
Stopping a batch prevents subsequent dispatches after the current request finishes, and cannot recall a message already accepted upstream.

## Session and storage boundaries

The main app retains its existing private Keychain credentials, including OAuth refresh tokens.
After successful private persistence it publishes only eligible access credentials into the dedicated shared Keychain item.
This preserves established logins during upgrade and leaves Remember Me semantics unchanged.
The extension never reads the main app's refresh credentials or rotates them.
Main-app recovery can pass a session-only access token transiently to a native upload without writing it to the shared Keychain.
Expired, missing, inaccessible, and session-only credentials require main-app recovery; the draft remains local.

The App Group contains protected, backup-excluded content and a credential-free server/session descriptor.
Only the main app establishes installation identity and runs its existing private reinstall cleanup.
The extension requires that identity before using inherited Keychain data.
Identity replacement/sign-out invalidates pending content; token renewal preserves its session generation.
Account removal invalidates matching records.
Native account cleanup records private retry intent before accessing shared storage; a successful server logout/removal remains successful when local cleanup must be retried.
Sign-out revokes the shared credential or commits a durable session fence before completing.
File deletion and inaccessible Keychain cleanup can be deferred; if neither revocation barrier can be committed, sign-out reports that specific failure and keeps the session active.
Late import, upload, and bridge completions cannot recreate invalidated records.

A cross-process file lock guards descriptors, quota reservations, revisions, and ownership fences.
Files and manifests are published atomically with filesystem synchronization.
Network and provider waits run outside that lock, and all locks, leases, copies, transfers, and response bodies are bounded.
Resume reconciliation reports incomplete imports, removes orphaned temporary data, and turns interrupted dispatches into unknown outcomes without replay.
Unreadable published metadata is retained and listed for explicit discard rather than silently erased.
Native uploads read protected files directly; the bridge only returns bounded chunks for thumbnail presentation.
Discard makes a bounded best-effort cleanup attempt for known unclaimed server uploads; server staging expiry remains the fallback.
Native sharing requires an HTTPS Axon server, validates certificates, refuses redirects, and retains the default iOS App Transport Security protections.
A configured HTTP server can still be used through the main app's existing transport, but incoming-share generation and sending preserve the draft and explain that HTTPS is required.
Diagnostics include share IDs, states, sizes, and fixed error categories; they exclude content, filenames, source URLs, and credentials.

## Contributor checks

From `clients/web`:

```sh
pnpm check:api
pnpm test src/incoming-share
pnpm test:sharing:native
pnpm test:sharing:simulator
pnpm lint
pnpm format:check
pnpm build
pnpm test:e2e
```

`check:api` also checks Swift wire schemas against OpenAPI.
Regenerate deliberately with `node scripts/share-contract.mjs --write`.
Swift and TypeScript consume the server's existing valid/invalid preview fixtures; Swift additionally round-trips the shared local manifest fixture.
The macOS workflow compiles the common module with iOS extension API restrictions and runs its XCTest suite on an iPhone simulator.
The simulator command requires Xcode, an installed iOS runtime, and `xcodegen` (`brew install xcodegen`).
It reuses a booted iPhone simulator or boots an available one; `AXON_SHARING_SIMULATOR=<UDID>` selects one explicitly.
It builds a temporary unsigned test runner, leaves Axon's installed app/session alone, and prints the retained `.xcresult` path.

| Layer                    | What it establishes                                                                                                                                     | Remaining limitation                                                                    |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| Swift and web unit tests | Revocation failures, ownership races, quotas, atomic preview attachment, schema parity, save batching, and cleanup outcomes                             | Simulated process/network failures and injected credential vault                        |
| Playwright               | Actual recovery UI, typing, wake hints, destination validation, HTTPS explanation, and no automatic resend across four browser configurations           | Native ownership bridge is substituted                                                  |
| iOS Simulator XCTest     | Actual iOS Foundation/filesystem APIs and native recovery transitions                                                                                   | Does not establish physical provisioning, shared Keychain access, or reinstall behavior |
| Signed iPhone/iPad       | App Group/Keychain entitlement behavior, lock/reinstall lifecycle, interrupted real transfers, keyboard, VoiceOver, Dynamic Type, and iPad multitasking | Requires deliberate device acceptance; live sends stay in Axon Testing                  |

The browser recovery fixture is a separate build entry, excluded from ordinary distribution builds:

```sh
AXON_SHARE_E2E=1 pnpm build
AXON_SHARE_E2E=1 AXON_WEB_E2E_PORT=4697 pnpm exec playwright test incoming-share.spec.ts --project=chromium --project=webkit-desktop
AXON_SHARE_E2E=1 MOBILE_E2E=1 AXON_WEB_E2E_PORT=4697 pnpm exec playwright test incoming-share.spec.ts --project=webkit-iphone
pnpm build
```

Restore the ordinary build before packaging.
The fixture substitutes only the native ownership/file bridge and exercises the actual recovery UI and send pipeline against the web suite's API stub.

## Signed device verification

Enable App Group `group.org.matrixaxon.axon.sharing` on the app ID `org.matrixaxon.axon` and the development-only probe ID `org.matrixaxon.axon.ShareProbe`.
Both profiles must grant the group and shared Keychain access.
After `tauri ios init`, run `node scripts/configure-ios-sharing.mjs`; `scripts/package-ios.sh` does this automatically after clean regeneration.
Set `AXON_SHARE_PROBE=1` to embed the probe while developing; distribution export methods reject this flag.

```sh
AXON_SHARE_PROBE=1 scripts/package-ios.sh --export-method debugging --install
```

Open Axon once to initialize installation/session access.
In Safari's share sheet choose **Axon sharing probe**, then **Check shared session**.
The probe reports App Group access, initialization, usable credential availability, and record count without exposing credentials.
**Save synthetic recovery draft** creates fixed local test content and sends nothing.
Return to Axon → Pending shares to review, retain, or discard it.
Any live send must target the dedicated **Axon Testing** room (`!SScJmZuEkBUnuydXdf:bostoncoop.net`).

The exported-artifact checker verifies signed entitlements against embedded profiles, app/extension versions, default ATS protections, and probe embedding.
Verify upgrades from a previously installed build preserve login, session-only login remains unavailable to the extension, and sign-out/server/account changes fence outstanding operations.
Verify removal/reinstall and extension access before the first app launch do not consume inherited Keychain credentials.
Record actual installation-marker behavior on iPhone; a simulator or a successful signature alone does not establish it.
Exercise cold/warm/suspended app states, process termination during import/upload/dispatch, offline and rejected authentication, quota/low-storage failure, and competing consumers.
Repeat recovery layout, multitasking, VoiceOver, and Dynamic Type checks on the signed iPhone and iPad.
Until that matrix is recorded, foundation implementation does not establish native-device acceptance or close issue [817](https://github.com/matrix-axon/matrix-axon/issues/817).

For the updated build, prioritize these manual checks:

1. Save a synthetic probe draft, type in recovery, immediately choose Keep draft, and reopen it.
   Background Axon for more than five minutes, return, and confirm ownership renews while the text survives and nothing sends automatically.
2. Sign out with a disposable pending draft and confirm the probe no longer finds a usable access credential and its pending-record count is zero.
   Sign back in and confirm a new synthetic draft can be recovered.
3. On iPad, review with the keyboard open, in split view, at large text sizes, and with VoiceOver.
   Confirm all controls remain reachable and delivery warnings are read.
4. Use Axon Testing for any deliberate send interruption; force-quit or interrupt connectivity during a request, relaunch, and inspect the saved outcomes before retrying.
   The outcome can already be confirmed if the response arrived before termination; an uncertain outcome must require explicit resend consent.
5. Use a disposable session for removal/reinstall and check the extension before the first app launch.
   This is the remaining physical test of inherited Keychain data and the app-owned installation marker.

## Verification recorded on 2026-10-30

On the linked iPhone, the signed probe reported an accessible App Group, initialized installation, and usable access credential.
The user created a synthetic draft in the extension, found it in Pending shares, and explicitly sent it successfully.
This establishes actual cross-process session access and the recovery-to-send path on the phone.
That user-run check predates the review fixes; repeat sign-out, resume, and recovery acceptance on the updated build.
The signed app also installed on the linked iPad; its recovery UI and accessibility checks remain pending.
The final probe build installed on both devices.
Clean project regeneration and signed IPA export passed for both the probe configuration and the default release configuration using the debugging export method.
The exported-artifact checks verified App Group and Keychain entitlements against provisioning profiles; the default IPA contains no extension bundle.

After review fixes, automated verification passed 30 Swift tests on both macOS and iOS Simulator, 2,772 web unit tests (two skipped), and 12 recovery UI checks across Chromium, Firefox, desktop WebKit, and iPhone WebKit.
The full Chromium run passed 247 tests with 11 skipped.
The unchanged server's preview contract and generation fixtures also passed against their production validators.
The earlier broader desktop browser run passed 734 tests with 30 skipped and three WebKit failures; isolated reruns passed two of those failures.
The remaining attachment-download event failure also reproduces on the untouched base commit `5b94c613` on this Mac.
These browser results do not replace the signed-device interruption, reinstall, low-storage, and accessibility checks above.
