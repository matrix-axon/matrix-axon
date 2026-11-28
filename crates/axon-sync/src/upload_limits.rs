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
/// busy gate counts as a failed lookup and backs off.
const ADMISSION_WAIT: Duration = Duration::from_secs(5);

/// The client a cached answer belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    homeserver: String,
    user_id: Option<String>,
    device_id: Option<String>,
}

impl Identity {
    fn of(client: &Client) -> Self {
        Self {
            homeserver: client.homeserver().to_string(),
            user_id: client.user_id().map(ToString::to_string),
            device_id: client.device_id().map(ToString::to_string),
        }
    }
}

#[derive(Debug)]
struct Entry {
    identity: Identity,
    /// The last successful answer and when it arrived.
    known: Option<(u64, Instant)>,
    /// When the last failed lookup ended.
    failed_at: Option<Instant>,
    /// A lookup is running.
    refreshing: bool,
    /// The homeserver refused an upload as too large since the last lookup,
    /// so look again even though the answer is fresh.
    refresh_due: bool,
}

impl Entry {
    fn new(identity: Identity) -> Self {
        Self {
            identity,
            known: None,
            failed_at: None,
            refreshing: false,
            refresh_due: false,
        }
    }
}

/// What [`HomeserverUploadLimitCache::read`] found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Read {
    /// The homeserver's limit, if a fresh one is known.
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
        let max_bytes = entry
            .known
            .filter(|(_, at)| now.saturating_duration_since(*at) < FRESH_FOR)
            .map(|(bytes, _)| bytes);
        let backing_off = entry
            .failed_at
            .is_some_and(|at| now.saturating_duration_since(at) < RETRY_AFTER);
        let start_refresh =
            !entry.refreshing && !backing_off && (max_bytes.is_none() || entry.refresh_due);
        if start_refresh {
            entry.refreshing = true;
        }
        Read {
            max_bytes,
            start_refresh,
        }
    }

    /// Record a lookup's result. `None` is a failed or unsupported lookup.
    /// A result for an identity that has since been replaced is dropped.
    pub(crate) fn finish(
        &self,
        account_id: Uuid,
        identity: &Identity,
        result: Option<u64>,
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
        entry.refresh_due = false;
        match result {
            Some(bytes) => {
                let previous = entry.known.map(|(previous, _)| previous);
                if previous != Some(bytes) {
                    tracing::info!(
                        %account_id,
                        homeserver_max_bytes = bytes,
                        previous_max_bytes = previous,
                        "upload limit: homeserver limit learned"
                    );
                }
                entry.known = Some((bytes, now));
                entry.failed_at = None;
            }
            None => entry.failed_at = Some(now),
        }
    }

    /// The homeserver (or a proxy in front of it) refused an upload as too
    /// large: look again on the next read, even if the answer is fresh.
    pub(crate) fn mark_refresh_due(&self, account_id: Uuid) {
        if let Some(entry) = self.lock().get_mut(&account_id) {
            entry.refresh_due = true;
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
    fn finish(mut self, result: Option<u64>) {
        self.done = true;
        self.cache
            .finish(self.account_id, &self.identity, result, Instant::now());
    }
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        if !self.done {
            self.cache
                .finish(self.account_id, &self.identity, None, Instant::now());
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
    /// known. Never waits: it reads only an already-connected client and the
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
        tracing::debug!(%account_id, error_kind = "gate_busy", "upload limit: lookup deferred by the homeserver load gate");
        guard.finish(None);
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
            None
        }
        Ok(Ok(bytes)) => {
            tracing::debug!(%account_id, homeserver_max_bytes = bytes, elapsed_ms, "upload limit: lookup succeeded");
            Some(bytes)
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
            None
        }
        Err(_) => {
            tracing::warn!(
                %account_id,
                timeout_secs = REQUEST_TIMEOUT.as_secs(),
                retry_secs = RETRY_AFTER.as_secs(),
                "upload limit: homeserver media config lookup timed out; using Axon's own cap"
            );
            None
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

        cache.finish(account, &id, Some(GIB), Instant::now());
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
        cache.finish(account, &id, Some(GIB), Instant::now());

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
        cache.finish(account, &id, None, Instant::now());

        let backing_off = cache.read(account, &id, Instant::now());
        assert_eq!(backing_off.max_bytes, None);
        assert!(!backing_off.start_refresh);

        tokio::time::advance(RETRY_AFTER).await;
        assert!(cache.read(account, &id, Instant::now()).start_refresh);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_refresh_keeps_a_still_fresh_answer() {
        let cache = HomeserverUploadLimitCache::default();
        let account = Uuid::new_v4();
        let id = identity("DEV1");
        cache.read(account, &id, Instant::now());
        cache.finish(account, &id, Some(GIB), Instant::now());

        // A refused upload asks for a lookup while the answer is fresh.
        cache.mark_refresh_due(account);
        let due = cache.read(account, &id, Instant::now());
        assert_eq!(due.max_bytes, Some(GIB));
        assert!(due.start_refresh);
        cache.finish(account, &id, None, Instant::now());

        assert_eq!(
            cache.read(account, &id, Instant::now()),
            Read {
                max_bytes: Some(GIB),
                start_refresh: false
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_identity_discards_the_old_answer_and_ignores_its_late_lookup() {
        let cache = HomeserverUploadLimitCache::default();
        let account = Uuid::new_v4();
        let old = identity("DEV1");
        let new = identity("DEV2");
        cache.read(account, &old, Instant::now());
        cache.finish(account, &old, Some(GIB), Instant::now());

        // Re-login: a new device, so a new entry.
        let read = cache.read(account, &new, Instant::now());
        assert_eq!(read.max_bytes, None);
        assert!(read.start_refresh);

        // A lookup the old client started finishes late: not credited.
        cache.finish(account, &old, Some(4096), Instant::now());
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
