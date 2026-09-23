# Axon Privacy Policy

**Effective date:** 2026-09-23

## Summary

Axon does not collect, store, transmit, or share any personal information, usage data, or analytics on behalf of its developer.
This app is a client for a self-hosted Axon server that you (or someone you trust) run and control.
All of your data — messages, media, contacts, and account credentials — is exchanged directly between this app and the Axon server address you configure.
The developer of this app has no server, database, or analytics service that receives your data, and no ability to access it.

## What this app does

Axon is a client application for the Axon personal server: a self-hosted backend that connects to your Matrix homeserver account(s) on your behalf.
When you use this app, you provide the network address of an Axon server that you or your organization operates.
The app communicates directly and only with that server.
The developer of this app does not operate, and has no access to, any Axon server, and cannot see, log, or intercept the data exchanged between this app and your server.

## Data we collect

**None.**
The developer does not collect any information from you or about your use of this app — no account information, no usage analytics, no advertising identifiers, no crash reports sent to a third party, and no device data of any kind.

## Data stored on your device

To function, the app stores some data locally on your device, none of which is ever sent to the developer:

- **Authentication credentials** for the Axon server you configure (e.g. an access token), stored locally in the app's storage.
- **A local cache** of messages, room state, and media, used to make the app responsive and to support offline viewing.
  This cache mirrors data already held by your Axon server and homeserver account.
- **Optional, on-device performance diagnostics**, if you enable this setting.
  These records contain only timing numbers and non-identifying labels — no message content, room names, or identifiers — and stay on your device unless you choose to export and share them yourself (for example, when reporting a bug).
  You can clear them at any time from the app's settings.

All of the above lives only on your device and, in the case of your account and message data, on the Axon server and Matrix homeserver you have connected the app to.
None of it passes through, or is visible to, the developer.

## Third parties

This app does not integrate any third-party analytics, advertising, or crash-reporting services.
It does not share data with any third party because it does not collect any to share.

## Your Axon server and homeserver

Because Axon is self-hosted, the operator of your Axon server and the operator of your Matrix homeserver (which may be you, your employer, or a provider you have chosen) are separately responsible for how that infrastructure handles your data.
This policy covers only the app itself; it does not extend to servers you or others operate and connect the app to.
If you did not set up your own Axon server, consult its operator's privacy policy or terms.

## Children's privacy

This app does not knowingly collect data from anyone, including children, because it does not collect data from anyone.

## Changes to this policy

If this policy changes, an updated version will be published at the same location in the project's source repository, with a revised effective date above.

## Contact

This app is developed and maintained as an open-source project.
For questions about this policy or the app's data practices, please open an issue at:

<https://github.com/matrix-axon/matrix-axon/issues>
