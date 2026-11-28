//! Each account's homeserver upload limit, `m.upload.size` (ADR 0143
//! section 6, issue #1210).
//!
//! Axon has its own staging cap, `media.max_upload_bytes`, and the homeserver
//! has another. A client can only use the lower of the two, so Axon reads the
//! homeserver's from the authenticated `GET /_matrix/client/v1/media/config`
//! and advertises the effective limit on account reads.
//!
//! Reads never wait on the network. [`UploadLimits::homeserver_max_bytes`]
//! answers from the cache, and when the answer is missing or older than
//! [`FRESH_FOR`] it starts one bounded background lookup for that account and
//! answers `None` until the lookup succeeds. A failed or unsupported lookup
//! backs off for [`RETRY_AFTER`]. `None` means "unknown", and callers fall back
//! to Axon's own cap.
//!
//! Every media upload reports a refusal as too large through
//! [`UploadRefusals`]. A homeserver's or proxy's `413` asks for a lookup even
//! while the answer is fresh. The SDK's own refusal names the `m.upload.size`
//! it cached on the client's first upload and keeps for the client's life; a
//! lookup cannot change it, so it caps what is advertised until the client is
//! rebuilt.
//!
//! The cache is shared state behind one std mutex, held only to read or write
//! an entry, never across an await. An entry belongs to one client identity
//! (homeserver URL, user ID and device ID). A re-login, which builds a client
//! with a new device, or a changed homeserver, starts a fresh entry, and a
//! lookup that finishes for a replaced identity is discarded rather than
//! credited to the new one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use matrix_sdk::config::RequestConfig;
use matrix_sdk::ruma::api::client::authenticated_media::get_media_config;
use matrix_sdk::{Client, HttpError};
use tokio::time::Instant;
use uuid::Uuid;

use crate::homeserver_gate::outcome_of;
use crate::manager::{ClientManager, ClientRequest};

/// How long a homeserver's answer is served before it is looked up again.
pub(crate) const FRESH_FOR: Duration = Duration::from_secs(60 * 60);
/// How long a failed or unsupported lookup waits before the next one.
pub(crate) const RETRY_AFTER: Duration = Duration::from_secs(60);
/// The lookup's own bound, once the load gate admits it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the lookup waits for the homeserver's load gate (ADR 0122). A
/// busy gate is not a failed lookup: the lookup never reached the homeserver,
/// so it is tried again after this long rather than after [`RETRY_AFTER`].
const ADMISSION_WAIT: Duration = Duration::from_secs(5);

/// The client a cached answer belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    homeserver: String,
    user_id: Option<String>,
    device_id: Option<String>,
}

impl Identity {
    pub(crate) fn of(client: &Client) -> Self {
        Self {
            homeserver: client.homeserver().to_string(),
            user_id: client.user_id().map(ToString::to_string),
            device_id: client.device_id().map(ToString::to_string),
        }
    }
}

/// How a lookup ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// The homeserver's `m.upload.size`.
    Answered(u64),
    /// The homeserver did not answer with a limit: an error, a timeout, an
    /// unsupported endpoint, or no `m.upload.size`. Backs off for
    /// [`RETRY_AFTER`].
    Failed,
    /// The homeserver's load gate did not admit the lookup. Tried again after
    /// [`ADMISSION_WAIT`].
    Deferred,
}

#[derive(Debug)]
struct Entry {
    identity: Identity,
    /// The last successful answer and when it arrived.
    known: Option<(u64, Instant)>,
    /// The `m.upload.size` the SDK client cached on its first upload, learned
    /// from its own refusal. The SDK keeps that value for the client's life
    /// and refuses anything above it before sending, so this caps what Axon
    /// advertises however high a later lookup answers.
    client_ceiling: Option<u64>,
    /// No lookup starts before this: a failed or deferred one backs off.
    retry_at: Option<Instant>,
    /// A lookup is running.
    refreshing: bool,
    /// The homeserver refused an upload as too large since the last
    /// successful lookup, so look again even though the answer is fresh. A
    /// failed lookup leaves it set, so the next try after the backoff still
    /// happens.
    refresh_due: bool,
}

impl Entry {
    fn new(identity: Identity) -> Self {
        Self {
            identity,
            known: None,
            client_ceiling: None,
            retry_at: None,
            refreshing: false,
            refresh_due: false,
        }
    }
}

/// What [`HomeserverUploadLimitCache::read`] found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Read {
    /// The homeserver's limit, if a fresh one is known, capped by what the
    /// SDK client will send.
    pub max_bytes: Option<u64>,
    /// The caller must start a lookup: it has been marked as running.
    pub start_refresh: bool,
}

/// The per-account cache. Cheap to clone; every clone shares one map, owned by
/// the [`ClientManager`] so the sync engine, the gateway and the account
/// reads all see the same answers.
#[derive(Clone, Default)]
pub(crate) struct HomeserverUploadLimitCache {
    entries: Arc<Mutex<HashMap<Uuid, Entry>>>,
}

impl HomeserverUploadLimitCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Entry>> {
        self.entries.lock().expect("upload limit cache poisoned")
    }

    /// The fresh answer for `identity`, and whether to start a lookup.
    pub(crate) fn read(&self, account_id: Uuid, identity: &Identity, now: Instant) -> Read {
        let mut entries = self.lock();
        let entry = entries
            .entry(account_id)
            .or_insert_with(|| Entry::new(identity.clone()));
        if entry.identity != *identity {
            tracing::debug!(%account_id, "upload limit: client identity changed; discarding the cached answer");
            *entry = Entry::new(identity.clone());
        }
        let fresh = entry
            .known
            .filter(|(_, at)| now.saturating_duration_since(*at) < FRESH_FOR)
            .map(|(bytes, _)| bytes);
        let backing_off = entry.retry_at.is_some_and(|at| now < at);
        let start_refresh =
            !entry.refreshing && !backing_off && (fresh.is_none() || entry.refresh_due);
        if start_refresh {
            entry.refreshing = true;
        }
        let max_bytes = match (fresh, entry.client_ceiling) {
            (Some(fresh), Some(ceiling)) => Some(fresh.min(ceiling)),
            (fresh, ceiling) => fresh.or(ceiling),
        };
        Read {
            max_bytes,
            start_refresh,
        }
    }

    /// Record a lookup's result. A result for an identity that has since been
    /// replaced is dropped.
    pub(crate) fn finish(
        &self,
        account_id: Uuid,
        identity: &Identity,
        result: Lookup,
        now: Instant,
    ) {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(&account_id) else {
            return;
        };
        if entry.identity != *identity {
            return;
        }
        entry.refreshing = false;
        match result {
            Lookup::Answered(bytes) => {
                let previous = entry.known.map(|(previous, _)| previous);
                if previous != Some(bytes) {
                    tracing::info!(
                        %account_id,
                        homeserver_max_bytes = bytes,
                        previous_max_bytes = previous,
                        client_ceiling_bytes = entry.client_ceiling,
                        "upload limit: homeserver limit learned"
                    );
                }
                entry.known = Some((bytes, now));
                entry.retry_at = None;
                entry.refresh_due = false;
            }
            Lookup::Failed => entry.retry_at = Some(now + RETRY_AFTER),
            Lookup::Deferred => entry.retry_at = Some(now + ADMISSION_WAIT),
        }
    }

    /// The homeserver (or a proxy in front of it) refused an upload as too
    /// large: look again on the next read, even if the answer is fresh.
    fn mark_refresh_due(&self, account_id: Uuid) {
        if let Some(entry) = self.lock().get_mut(&account_id) {
            entry.refresh_due = true;
        }
    }

    /// The SDK client refused an upload against the `m.upload.size` it
    /// cached, `max_bytes`. A lookup cannot change that value, so it is not
    /// a reason to look again; it caps what is advertised instead.
    fn cap_at_client_limit(&self, account_id: Uuid, max_bytes: u64) {
        if let Some(entry) = self.lock().get_mut(&account_id) {
            entry.client_ceiling =
                Some(entry.client_ceiling.map_or(max_bytes, |c| c.min(max_bytes)));
        }
    }

    /// Drop the account's entry along with its client: a rebuilt client has
    /// an empty SDK upload-size cache, so a ceiling learned from the old one
    /// no longer applies.
    pub(crate) fn forget(&self, account_id: Uuid) {
        self.lock().remove(&account_id);
    }
}

/// Where one account's media uploads report a refusal as too large (ADR 0143
/// section 6). The default records nothing, for a send built without an
/// account (a test).
#[derive(Clone, Default)]
pub(crate) struct UploadRefusals(Option<(HomeserverUploadLimitCache, Uuid)>);

impl UploadRefusals {
    pub(crate) fn for_account(cache: HomeserverUploadLimitCache, account_id: Uuid) -> Self {
        Self(Some((cache, account_id)))
    }

    /// The homeserver, or a proxy in front of it, answered an upload with a
    /// `413`: its limit may have dropped, so look it up again.
    pub(crate) fn homeserver_refused(&self) {
        if let Some((cache, account_id)) = &self.0 {
            tracing::warn!(%account_id, error_kind = "upload_too_large", "homeserver refused a media upload as too large; refreshing its upload limit");
            cache.mark_refresh_due(*account_id);
        }
    }

    /// The SDK refused an upload above the `m.upload.size` it cached for this
    /// client, `max_bytes`.
    pub(crate) fn client_refused(&self, max_bytes: u64) {
        if let Some((cache, account_id)) = &self.0 {
            tracing::warn!(%account_id, error_kind = "upload_too_large", client_max_bytes = max_bytes, "the SDK refused a media upload above the upload limit it cached; advertising that limit");
            cache.cap_at_client_limit(*account_id, max_bytes);
        }
    }
}

/// Ends a lookup as failed if its task is dropped before it records a result,
/// so the entry never stays marked as running.
struct RefreshGuard {
    cache: HomeserverUploadLimitCache,
    account_id: Uuid,
    identity: Identity,
    done: bool,
}

impl RefreshGuard {
    fn finish(mut self, result: Lookup) {
        self.done = true;
        self.cache
            .finish(self.account_id, &self.identity, result, Instant::now());
    }
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        if !self.done {
            self.cache.finish(
                self.account_id,
                &self.identity,
                Lookup::Failed,
                Instant::now(),
            );
        }
    }
}

/// The homeserver upload-limit port for the API layer and staging. Cheap to
/// clone.
#[derive(Clone)]
pub struct UploadLimits {
    manager: ClientManager,
}

impl UploadLimits {
    pub(crate) fn new(manager: ClientManager) -> Self {
        Self { manager }
    }

    /// The homeserver's `m.upload.size` for this account, if a fresh answer is
    /// known, or the one the SDK client cached where that is lower or nothing
    /// fresh is known (see the module docs).
    /// Never waits: it reads only an already-connected client and the
    /// cache, and starts a background lookup when the answer is missing or
    /// stale. `None` means unknown, including for an account with no
    /// connected client.
    pub fn homeserver_max_bytes(&self, account_id: Uuid) -> Option<u64> {
        let client = self.manager.cached(account_id)?;
        let identity = Identity::of(&client);
        let cache = self.manager.upload_limit_cache();
        let read = cache.read(account_id, &identity, Instant::now());
        if read.start_refresh {
            let guard = RefreshGuard {
                cache,
                account_id,
                identity,
                done: false,
            };
            let gate = self.manager.homeserver_gate(&client);
            tokio::spawn(refresh(guard, gate, client));
        }
        read.max_bytes
    }
}

/// One lookup under the homeserver's load gate. It holds the client's request
/// lease, so a logout waits at most for [`ADMISSION_WAIT`] plus
/// [`REQUEST_TIMEOUT`].
async fn refresh(
    guard: RefreshGuard,
    gate: Arc<crate::homeserver_gate::HomeserverGate>,
    client: ClientRequest<Client>,
) {
    let account_id = guard.account_id;
    let Some(permit) = gate.acquire_briefly(REQUEST_TIMEOUT, ADMISSION_WAIT).await else {
        tracing::debug!(%account_id, error_kind = "gate_busy", retry_secs = ADMISSION_WAIT.as_secs(), "upload limit: lookup deferred by the homeserver load gate");
        guard.finish(Lookup::Deferred);
        return;
    };
    let started = Instant::now();
    let result = tokio::time::timeout(REQUEST_TIMEOUT, fetch_upload_size(&client)).await;
    permit.finish(outcome_of(&result));
    drop(client);
    let elapsed_ms = started.elapsed().as_millis();
    let answer = match result {
        // A limit of 0 would refuse every upload, which no homeserver means.
        Ok(Ok(0)) => {
            tracing::warn!(%account_id, elapsed_ms, error_kind = "zero_limit", "upload limit: homeserver reported m.upload.size 0; using Axon's own cap");
            Lookup::Failed
        }
        Ok(Ok(bytes)) => {
            tracing::debug!(%account_id, homeserver_max_bytes = bytes, elapsed_ms, "upload limit: lookup succeeded");
            Lookup::Answered(bytes)
        }
        Ok(Err(error)) => {
            tracing::warn!(
                %account_id,
                elapsed_ms,
                status = error.as_client_api_error().map(|api| api.status_code.as_u16()),
                error = %error,
                retry_secs = RETRY_AFTER.as_secs(),
                "upload limit: homeserver media config lookup failed; using Axon's own cap"
            );
            Lookup::Failed
        }
        Err(_) => {
            tracing::warn!(
                %account_id,
                timeout_secs = REQUEST_TIMEOUT.as_secs(),
                retry_secs = RETRY_AFTER.as_secs(),
                "upload limit: homeserver media config lookup timed out; using Axon's own cap"
            );
            Lookup::Failed
        }
    };
    guard.finish(answer);
}

/// The homeserver's media config, without the SDK's retries: the
/// authenticated `GET /_matrix/client/v1/media/config` where the homeserver
/// supports it, else the legacy `/_matrix/media/v3/config`, the same choice
/// the SDK makes for its own pre-upload check. A response with no
/// `m.upload.size` fails to parse, which is the same "unknown" as an error.
async fn fetch_upload_size(client: &Client) -> Result<u64, HttpError> {
    use matrix_sdk::ruma::api::client::media;
    use matrix_sdk::ruma::api::Metadata;

    let config = RequestConfig::new()
        .disable_retry()
        .timeout(REQUEST_TIMEOUT);
    let versions = client.supported_versions().await?;
    let upload_size = if get_media_config::v1::Request::PATH_BUILDER.is_supported(&versions) {
        client
            .send(get_media_config::v1::Request::new())
            .with_request_config(config)
            .await?
            .upload_size
    } else {
        #[allow(deprecated)]
        client
            .send(media::get_media_config::v3::Request::new())
            .with_request_config(config)
            .await?
            .upload_size
    };
    Ok(u64::from(upload_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(device: &str) -> Identity {
        Identity {
            homeserver: "https://hs.example.org/".into(),
            user_id: Some("@alice:example.org".into()),
            device_id: Some(device.into()),
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[tokio::test(start_paused = true)]
    async fn a_miss_starts_one_lookup_and_serves_nothing_until_it_succeeds() {
        let cache = HomeserverUploadLimitCache::default();
        let account = Uuid::new_v4();
        let id = identity("DEV1");

        let first = cache.read(account, &id, Instant::now());
        assert_eq!(
            first,
            Read {
                max_bytes: None,
                start_refresh: true
            }
        );
        // One lookup per account at a time.
        let second = cache.read(account, &id, Instant::now());
        assert!(!second.start_refresh);
        assert_eq!(second.max_bytes, None);

        cache.finish(account, &id, Lookup::Answered(GIB), Instant::now());
        assert_eq!(
            cache.read(account, &id, Instant::now()),
            Read {
                max_bytes: Some(GIB),
                start_refresh: false
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_answer_is_not_served_and_starts_a_lookup() {
        let cache = HomeserverUploadLimitCache::default();
        let account = Uuid::new_v4();
        let id = identity("DEV1");
        cache.read(account, &id, Instant::now());
        cache.finish(account, &id, Lookup::Answered(GIB), Instant::now());

        tokio::time::advance(FRESH_FOR - Duration::from_secs(1)).await;
        assert_eq!(
            cache.read(account, &id, Instant::now()).max_bytes,
            Some(GIB)
        );

        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            cache.read(account, &id, Instant::now()),
            Read {
                max_bytes: None,
                start_refresh: true
            },
            "an hour-old answer falls back to Axon's own cap until a lookup succeeds"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_lookup_backs_off_for_a_minute() {
        let cache = HomeserverUploadLimitCache::default();
        let account = Uuid::new_v4();
        let id = identity("DEV1");
        assert!(cache.read(account, &id, Instant::now()).start_refresh);
        cache.finish(account, &id, Lookup::Failed, Instant::now());

        let backing_off = cache.read(account, &id, Instant::now());
        assert_eq!(backing_off.max_bytes, None);
        assert!(!backing_off.start_refresh);

        tokio::time::advance(RETRY_AFTER).await;
        assert!(cache.read(account, &id, Instant::now()).start_refresh);
    }

    /// Review of #1210: a lookup a `413` asked for that fails must not drop
    /// the request, or the disproven answer is served for the rest of the
    /// hour.
    #[tokio::test(start_paused = true)]
    async fn a_failed_refresh_keeps_a_still_fresh_answer_and_tries_again() {
        let cache = HomeserverUploadLimitCache::default();
        let account = Uuid::new_v4();
        let id = identity("DEV1");
        cache.read(account, &id, Instant::now());
        cache.finish(account, &id, Lookup::Answered(GIB), Instant::now());

        // A refused upload asks for a lookup while the answer is fresh.
        UploadRefusals::for_account(cache.clone(), account).homeserver_refused();
        let due = cache.read(account, &id, Instant::now());
        assert_eq!(due.max_bytes, Some(GIB));
        assert!(due.start_refresh);
        cache.finish(account, &id, Lookup::Failed, Instant::now());

        assert_eq!(
            cache.read(account, &id, Instant::now()),
            Read {
                max_bytes: Some(GIB),
                start_refresh: false
            },
            "backing off, still serving the fresh answer"
        );
        tokio::time::advance(RETRY_AFTER).await;
        let retry = cache.read(account, &id, Instant::now());
        assert!(
            retry.start_refresh,
            "the refused upload still asks for a lookup"
        );
        assert_eq!(retry.max_bytes, Some(GIB));

        // A success settles it.
        cache.finish(account, &id, Lookup::Answered(GIB / 2), Instant::now());
        tokio::time::advance(RETRY_AFTER).await;
        assert_eq!(
            cache.read(account, &id, Instant::now()),
            Read {
                max_bytes: Some(GIB / 2),
                start_refresh: false
            }
        );
    }

    /// A lookup the load gate turns away never reached the homeserver: it is
    /// not a failed attempt, so it tries again soon and keeps the request.
    #[tokio::test(start_paused = true)]
    async fn a_lookup_the_gate_defers_tries_again_soon() {
        let cache = HomeserverUploadLimitCache::default();
        let account = Uuid::new_v4();
        let id = identity("DEV1");
        cache.read(account, &id, Instant::now());
        cache.finish(account, &id, Lookup::Answered(GIB), Instant::now());
        UploadRefusals::for_account(cache.clone(), account).homeserver_refused();

        assert!(cache.read(account, &id, Instant::now()).start_refresh);
        cache.finish(account, &id, Lookup::Deferred, Instant::now());
        assert!(!cache.read(account, &id, Instant::now()).start_refresh);
        tokio::time::advance(ADMISSION_WAIT).await;
        assert!(
            cache.read(account, &id, Instant::now()).start_refresh,
            "a deferred lookup waits for the gate, not for a minute"
        );
    }

    /// Review of #1210: the SDK refuses above the `m.upload.size` it cached
    /// on the client's first upload, whatever a later lookup says, so its
    /// refusal caps the advertised limit until the client is rebuilt.
    #[tokio::test(start_paused = true)]
    async fn the_sdk_clients_own_limit_caps_the_answer_until_the_client_is_rebuilt() {
        let cache = HomeserverUploadLimitCache::default();
        let account = Uuid::new_v4();
        let id = identity("DEV1");
        let refusals = UploadRefusals::for_account(cache.clone(), account);
        cache.read(account, &id, Instant::now());
        // The operator raised the limit after the client first uploaded.
        cache.finish(account, &id, Lookup::Answered(GIB), Instant::now());
        refusals.client_refused(GIB / 4);
        assert_eq!(
            cache.read(account, &id, Instant::now()),
            Read {
                max_bytes: Some(GIB / 4),
                start_refresh: false
            },
            "a lookup cannot clear the SDK's cache, so it is not asked for"
        );

        // The cap outlives the hour; the lookup it starts cannot raise it.
        tokio::time::advance(FRESH_FOR).await;
        let stale = cache.read(account, &id, Instant::now());
        assert_eq!(stale.max_bytes, Some(GIB / 4));
        assert!(stale.start_refresh);
        cache.finish(account, &id, Lookup::Answered(GIB), Instant::now());
        assert_eq!(
            cache.read(account, &id, Instant::now()).max_bytes,
            Some(GIB / 4)
        );
        // A lower homeserver answer still wins.
        refusals.homeserver_refused();
        assert!(cache.read(account, &id, Instant::now()).start_refresh);
        cache.finish(account, &id, Lookup::Answered(GIB / 8), Instant::now());
        assert_eq!(
            cache.read(account, &id, Instant::now()).max_bytes,
            Some(GIB / 8)
        );

        // The client is rebuilt, with the same device: its SDK cache is empty.
        cache.forget(account);
        assert_eq!(
            cache.read(account, &id, Instant::now()),
            Read {
                max_bytes: None,
                start_refresh: true
            }
        );
        cache.finish(account, &id, Lookup::Answered(GIB), Instant::now());
        assert_eq!(
            cache.read(account, &id, Instant::now()).max_bytes,
            Some(GIB)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_identity_discards_the_old_answer_and_ignores_its_late_lookup() {
        let cache = HomeserverUploadLimitCache::default();
        let account = Uuid::new_v4();
        let old = identity("DEV1");
        let new = identity("DEV2");
        cache.read(account, &old, Instant::now());
        cache.finish(account, &old, Lookup::Answered(GIB), Instant::now());

        // Re-login: a new device, so a new entry.
        let read = cache.read(account, &new, Instant::now());
        assert_eq!(read.max_bytes, None);
        assert!(read.start_refresh);

        // A lookup the old client started finishes late: not credited.
        cache.finish(account, &old, Lookup::Answered(4096), Instant::now());
        let read = cache.read(account, &new, Instant::now());
        assert_eq!(read.max_bytes, None);
        assert!(!read.start_refresh, "the new identity's lookup still runs");

        let moved = Identity {
            homeserver: "https://other.example.org/".into(),
            ..new.clone()
        };
        assert!(cache.read(account, &moved, Instant::now()).start_refresh);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_lookup_counts_as_failed() {
        let cache = HomeserverUploadLimitCache::default();
        let account = Uuid::new_v4();
        let id = identity("DEV1");
        assert!(cache.read(account, &id, Instant::now()).start_refresh);
        drop(RefreshGuard {
            cache: cache.clone(),
            account_id: account,
            identity: id.clone(),
            done: false,
        });
        assert!(
            !cache.read(account, &id, Instant::now()).start_refresh,
            "backing off, not stuck refreshing"
        );
        tokio::time::advance(RETRY_AFTER).await;
        assert!(cache.read(account, &id, Instant::now()).start_refresh);
    }

    mod homeserver {
        use std::net::SocketAddr;

        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::get;
        use axum::{Json, Router};
        use matrix_sdk::ruma::api::MatrixVersion;
        use serde_json::{json, Value};

        use super::super::*;

        async fn serve(router: Router) -> SocketAddr {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            addr
        }

        async fn client_for(addr: SocketAddr, version: MatrixVersion) -> Client {
            use matrix_sdk::authentication::matrix::MatrixSession;
            use matrix_sdk::store::RoomLoadSettings;
            use matrix_sdk::{SessionMeta, SessionTokens};

            let client = Client::builder()
                .homeserver_url(format!("http://{addr}"))
                .server_versions([version])
                .build()
                .await
                .expect("client against the stand-in homeserver");
            client
                .matrix_auth()
                .restore_session(
                    MatrixSession {
                        meta: SessionMeta {
                            user_id: matrix_sdk::ruma::UserId::parse("@uploader:localhost")
                                .expect("test user id"),
                            device_id: "UPLOADDEV".into(),
                        },
                        tokens: SessionTokens {
                            access_token: "uploader-token".to_owned(),
                            refresh_token: None,
                        },
                    },
                    RoomLoadSettings::default(),
                )
                .await
                .expect("restore a session so the request is sent");
            client
        }

        fn config_route(path: &'static str, status: StatusCode, body: Value) -> Router {
            Router::new().route(
                path,
                get(move || {
                    let body = body.clone();
                    async move { (status, Json(body)).into_response() }
                }),
            )
        }

        const V1: &str = "/_matrix/client/v1/media/config";
        const V3: &str = "/_matrix/media/v3/config";

        #[tokio::test]
        async fn reads_m_upload_size_from_the_authenticated_endpoint() {
            let addr = serve(config_route(
                V1,
                StatusCode::OK,
                json!({"m.upload.size": 1_073_741_824u64}),
            ))
            .await;
            let client = client_for(addr, MatrixVersion::V1_11).await;
            assert_eq!(fetch_upload_size(&client).await.unwrap(), 1_073_741_824);
        }

        #[tokio::test]
        async fn falls_back_to_the_legacy_endpoint_on_an_older_homeserver() {
            let addr = serve(config_route(
                V3,
                StatusCode::OK,
                json!({"m.upload.size": 52_428_800u64}),
            ))
            .await;
            let client = client_for(addr, MatrixVersion::V1_1).await;
            assert_eq!(fetch_upload_size(&client).await.unwrap(), 52_428_800);
        }

        #[tokio::test]
        async fn an_error_or_a_missing_limit_is_unknown() {
            for (status, body) in [
                (
                    StatusCode::NOT_FOUND,
                    json!({"errcode": "M_UNRECOGNIZED", "error": "Unrecognized request"}),
                ),
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"errcode": "M_UNKNOWN", "error": "boom"}),
                ),
                // `m.upload.size` is optional in the spec; without it the
                // limit is unknown.
                (StatusCode::OK, json!({})),
            ] {
                let addr = serve(config_route(V1, status, body.clone())).await;
                let client = client_for(addr, MatrixVersion::V1_11).await;
                assert!(
                    fetch_upload_size(&client).await.is_err(),
                    "{status} {body} must not yield a limit"
                );
            }
        }
    }
}
