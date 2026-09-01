use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::search::SearchRequest;

const LIVE_RECONNECT_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const LIVE_RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Timeout for read-only / probe requests (list calls, timeline fetches, etc.).
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Message-mutation timeout. These routes can block on the sync engine acquiring
/// encryption keys; lifecycle operations such as recovery are deliberately not
/// capped here because their documented work can legitimately exceed 60 seconds.
const MESSAGE_MUTATION_TIMEOUT: Duration = Duration::from_secs(60);
/// Generous timeout for lifecycle operations (login, logout, recover, delete).
/// Recovery imports the megolm key backup and cross-signing keys, which can
/// legitimately exceed 60 s on a real account.
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Media downloads run on a shared worker pool, so a stalled response must not
/// hold one of those workers forever.
const MEDIA_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_MEDIA_BYTES: usize = 20 * 1024 * 1024;
/// Staging an upload legitimately takes longer than an ordinary mutation (it's
/// bounded by the whole file's transfer time, not a single small JSON body),
/// so it gets its own, more generous timeout.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct AxonClient {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl AxonClient {
    pub fn new(base_url: String, token: Option<String>) -> Self {
        let base_url = base_url.trim_end_matches('/').to_owned();
        let mut default_headers = HeaderMap::new();
        if let Some(ref t) = token {
            if let Ok(v) = HeaderValue::from_str(&bearer_value_str(t)) {
                default_headers.insert(AUTHORIZATION, v);
            }
        }
        let http = reqwest::ClientBuilder::new()
            .default_headers(default_headers)
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            base_url,
            token,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn has_bearer_token(&self) -> bool {
        self.token.is_some()
    }

    pub async fn list_rooms(&self, account_id: Option<Uuid>) -> Result<Vec<RoomDto>, ApiError> {
        let mut request = self.http.get(format!("{}/v1/rooms", self.base_url));
        if let Some(account_id) = account_id {
            request = request.query(&[("account_id", account_id)]);
        }
        self.send(read_request(request)).await
    }

    pub async fn list_accounts(&self) -> Result<Vec<AccountDto>, ApiError> {
        let request = self.http.get(format!("{}/v1/accounts", self.base_url));
        self.send(read_request(request)).await
    }

    /// Log a Matrix account in through Axon. `homeserver_url` is sent only when
    /// the caller supplies an override (the inline `/login` third argument);
    /// otherwise it is omitted and Axon resolves the canonical homeserver from
    /// the Matrix ID's server name (ADR 0023). Either way the TUI talks only to
    /// Axon.
    pub async fn login(
        &self,
        username: &str,
        password: &str,
        homeserver_url: Option<&str>,
    ) -> Result<AccountDto, ApiError> {
        let mut body = serde_json::json!({
            "username": username,
            "password": password,
        });
        if let Some(homeserver_url) = homeserver_url {
            body["homeserver_url"] = serde_json::Value::String(homeserver_url.to_owned());
        }
        let request = self
            .http
            .post(format!("{}/v1/accounts/login", self.base_url))
            .json(&body);
        self.send(lifecycle(request)).await
    }

    pub async fn logout(&self, account_id: Uuid) -> Result<AccountDto, ApiError> {
        let request = self
            .http
            .post(format!("{}/v1/accounts/{account_id}/logout", self.base_url));
        self.send(lifecycle(request)).await
    }

    pub async fn recover(
        &self,
        account_id: Uuid,
        recovery_key: &str,
    ) -> Result<AccountDto, ApiError> {
        let request = self
            .http
            .post(format!(
                "{}/v1/accounts/{account_id}/recover",
                self.base_url
            ))
            .json(&serde_json::json!({ "recovery_key": recovery_key }));
        self.send(lifecycle(request)).await
    }

    /// Originate, export-resume, or kick megolm backup upload (ADR 0098).
    /// `recovery_key` is required for create/export/replace; `None` is
    /// kick-upload only. The key is consumed once and never persisted.
    pub async fn enable_backup(
        &self,
        account_id: Uuid,
        recovery_key: Option<&str>,
    ) -> Result<EnableBackupResponse, ApiError> {
        let body = match recovery_key {
            Some(recovery_key) => serde_json::json!({ "recovery_key": recovery_key }),
            None => serde_json::json!({}),
        };
        let request = self
            .http
            .post(format!(
                "{}/v1/accounts/{account_id}/backup/enable",
                self.base_url
            ))
            .json(&body);
        self.send(lifecycle(request)).await
    }

    pub async fn delete_account(&self, account_id: Uuid) -> Result<(), ApiError> {
        let request = self
            .http
            .delete(format!("{}/v1/accounts/{account_id}", self.base_url));
        self.send_no_body(lifecycle(request)).await
    }

    pub async fn room_timeline(
        &self,
        account_id: Uuid,
        room_id: &str,
        cursor: Option<&str>,
        at_ts: Option<i64>,
        limit: usize,
    ) -> Result<TimelinePage, ApiError> {
        let mut request = self.http.get(format!(
            "{}/v1/accounts/{}/rooms/{}/timeline",
            self.base_url,
            account_id,
            path_segment(room_id)
        ));
        let limit = limit.to_string();
        request = request.query(&[("limit", limit.as_str())]);
        if let Some(cursor) = cursor {
            request = request.query(&[("cursor", cursor)]);
        }
        if let Some(at_ts) = at_ts {
            let at_ts = at_ts.to_string();
            request = request.query(&[("at_ts", at_ts.as_str())]);
        }
        self.send(read_request(request)).await
    }

    pub async fn search(&self, params: &SearchRequest) -> Result<SearchPage, ApiError> {
        let mut request = self.http.get(format!("{}/v1/search", self.base_url));
        request = request.query(&[("q", params.q.as_str())]);
        if let Some(account_id) = params.account_id {
            request = request.query(&[("account_id", account_id)]);
        }
        if let Some(room_id) = params.room_id.as_deref() {
            request = request.query(&[("room_id", room_id)]);
        }
        if let Some(sender) = params.sender.as_deref() {
            request = request.query(&[("sender", sender)]);
        }
        if let Some(from) = params.from {
            request = request.query(&[("from", from)]);
        }
        if let Some(to) = params.to {
            request = request.query(&[("to", to)]);
        }
        let limit = params.limit.to_string();
        request = request.query(&[("limit", limit.as_str())]);
        if let Some(cursor) = params.cursor.as_deref() {
            request = request.query(&[("cursor", cursor)]);
        }
        self.send(read_request(request)).await
    }

    /// Read one namespace of per-device state (M12): the server's last-write-
    /// wins merged view across all the account's devices, tombstoned keys
    /// absent. `device_id` names this client install; the merge is account-wide.
    pub async fn get_device_state(
        &self,
        device_id: Uuid,
        account_id: Uuid,
        namespace: &str,
    ) -> Result<DeviceStateDto, ApiError> {
        let request = self
            .http
            .get(format!(
                "{}/v1/devices/{device_id}/state/{}",
                self.base_url,
                path_segment(namespace)
            ))
            .query(&[("account_id", account_id)]);
        self.send(read_request(request)).await
    }

    /// Merge-upsert per-device state (M12). Only the keys in `entries` are
    /// touched; a `None` value deletes the key (a server-side tombstone so the
    /// deletion wins the cross-device merge). The change fans out to sibling
    /// devices as a `device_state.changed` WS frame carrying this `device_id`.
    pub async fn put_device_state(
        &self,
        device_id: Uuid,
        account_id: Uuid,
        namespace: &str,
        entries: &HashMap<String, Option<Value>>,
    ) -> Result<(), ApiError> {
        let request = self
            .http
            .put(format!(
                "{}/v1/devices/{device_id}/state/{}",
                self.base_url,
                path_segment(namespace)
            ))
            .query(&[("account_id", account_id)])
            .json(&serde_json::json!({ "entries": entries }));
        // The response carries only the server's `updated_at`, which this
        // client doesn't consume (server-clock LWW).
        let _: Value = self.send(read_request(request)).await?;
        Ok(())
    }

    pub async fn room_members(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<Vec<MemberDto>, ApiError> {
        let request = self.http.get(format!(
            "{}/v1/accounts/{}/rooms/{}/members",
            self.base_url,
            account_id,
            path_segment(room_id)
        ));
        self.send(read_request(request)).await
    }

    pub async fn send_message(
        &self,
        account_id: Uuid,
        room_id: &str,
        body: &str,
        formatted: Option<(&str, &str)>,
        relation: SendRelation<'_>,
    ) -> Result<SendResultDto, ApiError> {
        let request = self.http.post(format!(
            "{}/v1/accounts/{}/rooms/{}/send",
            self.base_url,
            account_id,
            path_segment(room_id)
        ));
        let mut payload = serde_json::json!({ "body": body });
        if let Some((fmt, fb)) = formatted {
            payload["format"] = serde_json::json!(fmt);
            payload["formatted_body"] = serde_json::json!(fb);
        }
        // ADR 0032 M4 send contract: the server builds the `m.relates_to` envelope
        // from these convenience fields (plain reply, or `m.thread` membership).
        if let Some(reply_to) = relation.reply_to {
            payload["reply_to"] = serde_json::json!(reply_to);
        }
        if let Some(thread_root) = relation.thread_root {
            payload["thread_root"] = serde_json::json!(thread_root);
        }
        let request = message_mutation(request).json(&payload);
        self.send(request).await
    }

    pub async fn edit_message(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        body: &str,
        formatted: Option<(&str, &str)>,
    ) -> Result<SendResultDto, ApiError> {
        let request = self.http.put(format!(
            "{}/v1/accounts/{}/rooms/{}/events/{}",
            self.base_url,
            account_id,
            path_segment(room_id),
            path_segment(event_id)
        ));
        let mut payload = serde_json::json!({ "body": body });
        if let Some((fmt, fb)) = formatted {
            payload["format"] = serde_json::json!(fmt);
            payload["formatted_body"] = serde_json::json!(fb);
        }
        let request = message_mutation(request).json(&payload);
        self.send(request).await
    }

    pub async fn redact_event(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        reason: Option<&str>,
    ) -> Result<SendResultDto, ApiError> {
        let request = self.http.delete(format!(
            "{}/v1/accounts/{}/rooms/{}/events/{}",
            self.base_url,
            account_id,
            path_segment(room_id),
            path_segment(event_id)
        ));
        let mut request = message_mutation(request);
        if let Some(reason) = reason {
            request = request.query(&[("reason", reason)]);
        }
        self.send(request).await
    }

    pub async fn react(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        key: &str,
    ) -> Result<SendResultDto, ApiError> {
        let request = self.http.post(format!(
            "{}/v1/accounts/{}/rooms/{}/events/{}/reactions",
            self.base_url,
            account_id,
            path_segment(room_id),
            path_segment(event_id)
        ));
        let request = message_mutation(request).json(&serde_json::json!({ "key": key }));
        self.send(request).await
    }

    /// Send a real Matrix read receipt to the homeserver (ADR 0067,
    /// `POST …/rooms/{room_id}/read`): sets both the public `m.read` receipt and
    /// the private `m.fully_read` marker to `event_id`. Best-effort — the caller
    /// fires this alongside the internal read-marker PUT and never surfaces its
    /// failure. Empty success body.
    pub async fn send_read_receipt(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
    ) -> Result<(), ApiError> {
        let request = self
            .http
            .post(format!(
                "{}/v1/accounts/{}/rooms/{}/read",
                self.base_url,
                account_id,
                path_segment(room_id)
            ))
            .json(&serde_json::json!({ "event_id": event_id }));
        self.send_no_body(message_mutation(request)).await
    }

    /// Send a typing notice to the homeserver (ADR 0068 M19a,
    /// `PUT …/rooms/{room_id}/typing`). `typing = false` clears it early.
    /// Best-effort fire-and-forget; empty success body.
    pub async fn send_typing_notice(
        &self,
        account_id: Uuid,
        room_id: &str,
        typing: bool,
    ) -> Result<(), ApiError> {
        let request = self
            .http
            .put(format!(
                "{}/v1/accounts/{}/rooms/{}/typing",
                self.base_url,
                account_id,
                path_segment(room_id)
            ))
            .json(&serde_json::json!({ "typing": typing }));
        self.send_no_body(message_mutation(request)).await
    }

    pub async fn leave_room(&self, account_id: Uuid, room_id: &str) -> Result<(), ApiError> {
        self.room_membership_no_body(account_id, room_id, "leave")
            .await
    }

    pub async fn forget_room(&self, account_id: Uuid, room_id: &str) -> Result<(), ApiError> {
        self.room_membership_no_body(account_id, room_id, "forget")
            .await
    }

    pub async fn invite_user(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
    ) -> Result<(), ApiError> {
        let request = self
            .room_membership_request(account_id, room_id, "invite")
            .json(&serde_json::json!({ "user_id": user_id }));
        self.send_no_body(message_mutation(request)).await
    }

    pub async fn kick_user(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), ApiError> {
        self.moderate_user(account_id, room_id, "kick", user_id, reason)
            .await
    }

    pub async fn ban_user(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), ApiError> {
        self.moderate_user(account_id, room_id, "ban", user_id, reason)
            .await
    }

    pub async fn unban_user(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), ApiError> {
        self.moderate_user(account_id, room_id, "unban", user_id, reason)
            .await
    }

    async fn room_membership_no_body(
        &self,
        account_id: Uuid,
        room_id: &str,
        verb: &str,
    ) -> Result<(), ApiError> {
        let request = self.room_membership_request(account_id, room_id, verb);
        self.send_no_body(message_mutation(request)).await
    }

    fn room_membership_request(
        &self,
        account_id: Uuid,
        room_id: &str,
        verb: &str,
    ) -> reqwest::RequestBuilder {
        self.http.post(format!(
            "{}/v1/accounts/{}/rooms/{}/{}",
            self.base_url,
            account_id,
            path_segment(room_id),
            verb
        ))
    }

    async fn moderate_user(
        &self,
        account_id: Uuid,
        room_id: &str,
        verb: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), ApiError> {
        let request = self.room_membership_request(account_id, room_id, verb);
        let mut payload = serde_json::json!({ "user_id": user_id });
        if let Some(reason) = reason.filter(|reason| !reason.trim().is_empty()) {
            payload["reason"] = serde_json::json!(reason);
        }
        self.send_no_body(message_mutation(request).json(&payload))
            .await
    }

    /// Stage raw upload bytes ahead of a `send_media` call (ADR 0059/0062,
    /// `POST …/media/uploads`). `kind` is `"image"` or `"file"`; the server
    /// rejects `"image"` unless `content_type` is `image/*`. `filename` is
    /// normalized server-side to its basename.
    pub async fn stage_upload(
        &self,
        account_id: Uuid,
        kind: &str,
        filename: &str,
        content_type: Option<&str>,
        bytes: Vec<u8>,
    ) -> Result<StagedUploadDto, ApiError> {
        let mut request = self
            .http
            .post(format!(
                "{}/v1/accounts/{}/media/uploads",
                self.base_url, account_id
            ))
            .query(&[("kind", kind), ("filename", filename)])
            .timeout(UPLOAD_TIMEOUT)
            .body(bytes);
        if let Some(content_type) = content_type {
            request = request.header(reqwest::header::CONTENT_TYPE, content_type);
        }
        self.send(request).await
    }

    /// Send a previously staged upload into a room (ADR 0059/0062,
    /// `POST …/rooms/{room_id}/send-media`), claiming `upload_id`.
    pub async fn send_media(
        &self,
        account_id: Uuid,
        room_id: &str,
        upload_id: Uuid,
        caption: Option<&str>,
        relation: SendRelation<'_>,
    ) -> Result<SendResultDto, ApiError> {
        let request = self.http.post(format!(
            "{}/v1/accounts/{}/rooms/{}/send-media",
            self.base_url,
            account_id,
            path_segment(room_id)
        ));
        let mut payload = serde_json::json!({ "upload_id": upload_id });
        if let Some(caption) = caption {
            payload["caption"] = serde_json::json!(caption);
        }
        if let Some(reply_to) = relation.reply_to {
            payload["reply_to"] = serde_json::json!(reply_to);
        }
        if let Some(thread_root) = relation.thread_root {
            payload["thread_root"] = serde_json::json!(thread_root);
        }
        let request = message_mutation(request).json(&payload);
        self.send(request).await
    }

    /// Download media identified by an `mxc://` URI, routed through the Axon
    /// server's authenticated media proxy. The server name and media ID are
    /// extracted from the URI and placed in the path. Responses larger than
    /// 20 MiB are refused so one event cannot exhaust the TUI's memory.
    pub async fn get_media(&self, account_id: Uuid, mxc_url: &str) -> Result<Vec<u8>, ApiError> {
        let rest = mxc_url
            .strip_prefix("mxc://")
            .ok_or_else(|| ApiError::Url("not an mxc:// URI".to_owned()))?;
        let (server, media_id) = rest
            .split_once('/')
            .ok_or_else(|| ApiError::Url("malformed mxc:// URI".to_owned()))?;
        if server.is_empty() || media_id.is_empty() {
            return Err(ApiError::Url("malformed mxc:// URI".to_owned()));
        }
        let request = media_request(self.http.get(format!(
            "{}/v1/media/{}/{}/{}",
            self.base_url,
            account_id,
            path_segment(server),
            path_segment(media_id),
        )));
        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            if response
                .content_length()
                .is_some_and(|length| length > MAX_MEDIA_BYTES as u64)
            {
                return Err(ApiError::Url(format!(
                    "media exceeds {} MiB limit",
                    MAX_MEDIA_BYTES / 1024 / 1024
                )));
            }
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                if bytes.len().saturating_add(chunk.len()) > MAX_MEDIA_BYTES {
                    return Err(ApiError::Url(format!(
                        "media exceeds {} MiB limit",
                        MAX_MEDIA_BYTES / 1024 / 1024
                    )));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        } else {
            let text = response.text().await?;
            Err(ApiError::Status {
                status,
                message: text,
            })
        }
    }

    pub async fn get_event(&self, account_id: Uuid, event_id: &str) -> Result<EventDto, ApiError> {
        let request = self.http.get(format!(
            "{}/v1/accounts/{}/events/{}",
            self.base_url,
            account_id,
            path_segment(event_id)
        ));
        self.send(read_request(request)).await
    }

    /// List the room's thread roots, most-recently-active first (ADR 0032 M3).
    /// An unknown room or a room with no threads yields an empty list.
    pub async fn room_threads(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<Vec<ThreadSummaryDto>, ApiError> {
        let request = self.http.get(format!(
            "{}/v1/accounts/{}/rooms/{}/threads",
            self.base_url,
            account_id,
            path_segment(room_id)
        ));
        self.send(read_request(request)).await
    }

    /// A page of a single thread's members, newest first, reusing the room
    /// timeline's opaque cursor pagination (ADR 0032 M3).
    pub async fn thread_timeline(
        &self,
        account_id: Uuid,
        room_id: &str,
        root_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<TimelinePage, ApiError> {
        let mut request = self.http.get(format!(
            "{}/v1/accounts/{}/rooms/{}/threads/{}/timeline",
            self.base_url,
            account_id,
            path_segment(room_id),
            path_segment(root_id)
        ));
        let limit = limit.to_string();
        request = request.query(&[("limit", limit.as_str())]);
        if let Some(cursor) = cursor {
            request = request.query(&[("cursor", cursor)]);
        }
        self.send(read_request(request)).await
    }

    /// Start an outgoing SAS flow (ADR 0028 §1, ADR 0040). The target is either a
    /// `device_id` (self-verification of the account's own device) or a `user_id`
    /// (cross-user verification of another user). The returned `flow_id` is stable
    /// for the flow's lifetime.
    pub async fn start_verification(
        &self,
        account_id: Uuid,
        user_id: Option<&str>,
        device_id: Option<&str>,
    ) -> Result<StartVerifyResponse, ApiError> {
        let mut body = serde_json::Map::new();
        if let Some(user_id) = user_id {
            body.insert("user_id".to_owned(), user_id.into());
        }
        if let Some(device_id) = device_id {
            body.insert("device_id".to_owned(), device_id.into());
        }
        let request = self
            .http
            .post(format!("{}/v1/accounts/{account_id}/verify", self.base_url))
            .json(&serde_json::Value::Object(body));
        self.send(lifecycle(request)).await
    }

    /// List a user's Matrix devices (`GET …/accounts/{id}/devices`, M16).
    pub async fn list_devices(&self, account_id: Uuid) -> Result<DeviceListDto, ApiError> {
        let request = self.http.get(format!(
            "{}/v1/accounts/{account_id}/devices",
            self.base_url
        ));
        self.send(read_request(request)).await
    }

    /// List the account's active/recent verification flows. Used on reconnect to
    /// discover a request that arrived while disconnected (ADR 0028 §3).
    pub async fn list_flows(&self, account_id: Uuid) -> Result<Vec<FlowDto>, ApiError> {
        let request = self
            .http
            .get(format!("{}/v1/accounts/{account_id}/verify", self.base_url));
        self.send(read_request(request)).await
    }

    /// Re-read one flow's state. A 404 (see [`ApiError::is_not_found`]) means the
    /// server has no record of the flow — treated as an implicit cancellation by
    /// the caller (ADR 0028 §3).
    pub async fn get_flow(&self, account_id: Uuid, flow_id: &str) -> Result<FlowDto, ApiError> {
        let request = self.http.get(format!(
            "{}/v1/accounts/{}/verify/{}",
            self.base_url,
            account_id,
            path_segment(flow_id)
        ));
        self.send(read_request(request)).await
    }

    /// Confirm the SAS values match. Idempotent server-side.
    pub async fn confirm_verification(
        &self,
        account_id: Uuid,
        flow_id: &str,
    ) -> Result<(), ApiError> {
        let request = self.http.post(format!(
            "{}/v1/accounts/{}/verify/{}/confirm",
            self.base_url,
            account_id,
            path_segment(flow_id)
        ));
        self.send_no_body(lifecycle(request)).await
    }

    /// Cancel the flow. Idempotent server-side (safe on already-terminal flows).
    pub async fn cancel_verification(
        &self,
        account_id: Uuid,
        flow_id: &str,
    ) -> Result<(), ApiError> {
        let request = self.http.post(format!(
            "{}/v1/accounts/{}/verify/{}/cancel",
            self.base_url,
            account_id,
            path_segment(flow_id)
        ));
        self.send_no_body(lifecycle(request)).await
    }

    /// Fetch the per-event verification bundle (M7c / ADR 0031): the at-decrypt
    /// snapshot plus live cross-signing evidence. Returned raw for display.
    pub async fn get_verification_bundle(
        &self,
        account_id: Uuid,
        event_id: &str,
    ) -> Result<Value, ApiError> {
        let request = self.http.get(format!(
            "{}/v1/accounts/{}/events/{}/verification",
            self.base_url,
            account_id,
            path_segment(event_id)
        ));
        self.send(read_request(request)).await
    }

    async fn send<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, ApiError> {
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        if status.is_success() {
            let envelope: ApiResponse<T> = serde_json::from_str(&text)?;
            Ok(envelope.data)
        } else {
            let message = serde_json::from_str::<ErrorResponse>(&text)
                .map(|body| format!("{}: {}", body.error.code, body.error.message))
                .unwrap_or_else(|_| text);
            Err(ApiError::Status { status, message })
        }
    }

    async fn send_no_body(&self, request: reqwest::RequestBuilder) -> Result<(), ApiError> {
        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            let text = response.text().await?;
            let message = serde_json::from_str::<ErrorResponse>(&text)
                .map(|body| format!("{}: {}", body.error.code, body.error.message))
                .unwrap_or_else(|_| text);
            Err(ApiError::Status { status, message })
        }
    }

    pub fn ws_url(&self) -> Result<String, ApiError> {
        let url =
            reqwest::Url::parse(&self.base_url).map_err(|err| ApiError::Url(err.to_string()))?;
        let scheme = match url.scheme() {
            "http" => "ws",
            "https" => "wss",
            other => return Err(ApiError::UnsupportedScheme(other.to_owned())),
        };
        let mut url = url;
        url.set_scheme(scheme)
            .map_err(|_| ApiError::UnsupportedScheme(scheme.to_owned()))?;
        url.set_path("/v1/ws");
        url.set_query(None);
        Ok(url.to_string())
    }
}

fn read_request(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request.timeout(HTTP_REQUEST_TIMEOUT)
}

fn lifecycle(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request.timeout(LIFECYCLE_TIMEOUT)
}

fn message_mutation(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request.timeout(MESSAGE_MUTATION_TIMEOUT)
}

fn media_request(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request.timeout(MEDIA_TIMEOUT)
}

pub(crate) fn bearer_value_str(token: &str) -> String {
    format!("Bearer {token}")
}

pub async fn websocket_task(client: AxonClient, tx: mpsc::UnboundedSender<LiveFrame>) {
    let url = match client.ws_url() {
        Ok(url) => url,
        Err(err) => {
            let _ = tx.send(LiveFrame::Disconnected(err.to_string()));
            return;
        }
    };

    let mut backoff = LIVE_RECONNECT_INITIAL_BACKOFF;
    loop {
        let req_result: Result<_, String> = (|| {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION as WS_AUTHORIZATION;
            use tokio_tungstenite::tungstenite::http::HeaderValue as WsHeaderValue;
            let mut req = url
                .as_str()
                .into_client_request()
                .map_err(|e| e.to_string())?;
            if let Some(t) = client.token.as_deref() {
                let value =
                    WsHeaderValue::from_str(&bearer_value_str(t)).map_err(|e| e.to_string())?;
                req.headers_mut().insert(WS_AUTHORIZATION, value);
            }
            Ok(req)
        })();
        let reason = match req_result {
            Err(e) => e,
            Ok(req) => match tokio_tungstenite::connect_async(req).await {
                Ok((mut socket, _)) => {
                    let _ = tx.send(LiveFrame::Connected);
                    backoff = LIVE_RECONNECT_INITIAL_BACKOFF;
                    read_websocket(&mut socket, &tx).await
                }
                Err(err) => err.to_string(),
            },
        };

        let _ = tx.send(LiveFrame::Reconnecting {
            reason,
            delay: backoff,
        });
        sleep(backoff).await;
        backoff = next_live_reconnect_backoff(backoff);
    }
}

async fn read_websocket<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    tx: &mpsc::UnboundedSender<LiveFrame>,
) -> String
where
    tokio_tungstenite::WebSocketStream<S>:
        futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(frame) = socket.next().await {
        match frame {
            Ok(Message::Text(text)) => {
                if let Some(frame) = decode_ws_frame(&text) {
                    let _ = tx.send(frame);
                }
            }
            Ok(Message::Close(_)) => return "websocket closed".to_owned(),
            Ok(_) => {}
            Err(err) => return err.to_string(),
        }
    }
    "websocket closed".to_owned()
}

/// Decode a single `/v1/ws` text frame into the [`LiveFrame`] to forward, or
/// `None` for a frame kind the client does not consume (forward-compatibility:
/// an unknown `type` is ignored, never an error). Malformed JSON or a payload
/// that does not match its declared `type` becomes a [`LiveFrame::ProtocolError`]
/// so the failure is visible rather than silent.
fn decode_ws_frame(text: &str) -> Option<LiveFrame> {
    let envelope: WsEnvelope<Value> = match serde_json::from_str(text) {
        Ok(envelope) => envelope,
        Err(err) => return Some(LiveFrame::ProtocolError(err.to_string())),
    };
    let kind = match VerificationFrameKind::from_type(&envelope.kind) {
        Some(kind) => {
            let payload: VerificationFrameDto = match serde_json::from_value(envelope.payload) {
                Ok(payload) => payload,
                Err(err) => return Some(LiveFrame::ProtocolError(err.to_string())),
            };
            return Some(LiveFrame::Verification(VerificationFrame {
                account_id: envelope.account_id,
                kind,
                payload,
            }));
        }
        None => envelope.kind.as_str(),
    };
    match kind {
        "timeline.event" => {
            let event: EventDto = match serde_json::from_value(envelope.payload) {
                Ok(event) => event,
                Err(err) => return Some(LiveFrame::ProtocolError(err.to_string())),
            };
            if envelope.account_id != event.account_id {
                return Some(LiveFrame::ProtocolError(
                    "live frame account_id did not match payload".to_owned(),
                ));
            }
            Some(LiveFrame::Timeline(Box::new(event)))
        }
        "sender_trust.violation" => {
            let payload: SenderTrustViolationDto = match serde_json::from_value(envelope.payload) {
                Ok(payload) => payload,
                Err(err) => return Some(LiveFrame::ProtocolError(err.to_string())),
            };
            Some(LiveFrame::SenderTrustViolation {
                account_id: envelope.account_id,
                payload,
            })
        }
        "device_state.changed" => {
            let payload: DeviceStateChangedDto = match serde_json::from_value(envelope.payload) {
                Ok(payload) => payload,
                Err(err) => return Some(LiveFrame::ProtocolError(err.to_string())),
            };
            Some(LiveFrame::DeviceState {
                account_id: envelope.account_id,
                payload,
            })
        }
        "ephemeral.passthrough" => {
            let payload: EphemeralPassthroughDto = match serde_json::from_value(envelope.payload) {
                Ok(payload) => payload,
                Err(err) => return Some(LiveFrame::ProtocolError(err.to_string())),
            };
            Some(LiveFrame::Ephemeral {
                account_id: envelope.account_id,
                payload,
            })
        }
        _ => None,
    }
}

fn next_live_reconnect_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(LIVE_RECONNECT_MAX_BACKOFF)
}

#[derive(Debug)]
pub enum LiveFrame {
    Connected,
    Reconnecting {
        reason: String,
        delay: Duration,
    },
    Disconnected(String),
    ProtocolError(String),
    Timeline(Box<EventDto>),
    /// A `verification.*` SAS frame (ADR 0027/0028). The lossy broadcast bus may
    /// drop frames, so the client treats these as hints and re-reads
    /// `GET …/verify/{flow_id}` on reconnect (ADR 0028 §3).
    Verification(VerificationFrame),
    /// A `sender_trust.violation` overlay frame (ADR 0031 / M7c).
    SenderTrustViolation {
        account_id: Uuid,
        payload: SenderTrustViolationDto,
    },
    /// A `device_state.changed` frame (M12, ADR 0048): another device wrote
    /// drafts / read markers. Frames carrying this client's own `device_id`
    /// are its own PUTs echoed back and must be ignored (echo suppression).
    DeviceState {
        account_id: Uuid,
        payload: DeviceStateChangedDto,
    },
    /// An `ephemeral.passthrough` frame (M18, ADR 0056): an allowlisted raw
    /// ephemeral event (typing, receipts) forwarded from the homeserver. Lossy
    /// like the rest of the bus, so typing overlays are cleared on reconnect.
    Ephemeral {
        account_id: Uuid,
        payload: EphemeralPassthroughDto,
    },
}

/// Which `verification.*` frame this is. Mirrors the server's frame-kind tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationFrameKind {
    Requested,
    Sas,
    Done,
    Cancelled,
}

impl VerificationFrameKind {
    /// Map a `/v1/ws` envelope `type` tag to its kind, or `None` if the tag is
    /// not a verification frame.
    fn from_type(kind: &str) -> Option<Self> {
        match kind {
            "verification.requested" => Some(Self::Requested),
            "verification.sas" => Some(Self::Sas),
            "verification.done" => Some(Self::Done),
            "verification.cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// A decoded `verification.*` frame: its kind plus the wire payload.
#[derive(Debug, Clone)]
pub struct VerificationFrame {
    pub account_id: Uuid,
    pub kind: VerificationFrameKind,
    pub payload: VerificationFrameDto,
}

/// The wire payload shared by all `verification.*` frames. Fields that don't
/// apply to a given stage are omitted by the server: `emoji`/`decimals` only on
/// `verification.sas`, `reason` only on `verification.cancelled`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct VerificationFrameDto {
    pub flow_id: String,
    /// The user being verified — own user id (self-verification) or the peer's
    /// (cross-user, ADR 0040). Defaulted for forward-compatibility with servers
    /// that predate the field.
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub emoji: Option<Vec<EmojiDto>>,
    #[serde(default)]
    pub decimals: Option<[u16; 3]>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// The wire payload for a `sender_trust.violation` frame.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SenderTrustViolationDto {
    pub user_id: String,
    #[serde(default)]
    pub verification_violation: bool,
}

/// The wire payload for a `device_state.changed` frame (M12): the originating
/// device, the namespace, and the written entries — a JSON `null` value means
/// the key was deleted on that device. The wire also carries `updated_at`,
/// which this client doesn't consume (server-clock LWW needs no client-side
/// timestamp comparison).
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceStateChangedDto {
    pub device_id: Uuid,
    pub namespace: String,
    #[serde(default)]
    pub entries: HashMap<String, Value>,
}

/// The wire payload for an `ephemeral.passthrough` frame (M18, ADR 0056): an
/// allowlisted raw Matrix ephemeral event (`m.typing`, `m.receipt`) forwarded
/// verbatim. `room_id` is absent for account-scoped signals (none today);
/// `content` is the untouched Matrix `content`, parsed by the ephemeral store.
#[derive(Debug, Clone, Deserialize)]
pub struct EphemeralPassthroughDto {
    #[serde(default)]
    pub room_id: Option<String>,
    pub event_type: String,
    #[serde(default)]
    pub content: Value,
}

/// One namespace of per-device state as returned by
/// `GET /v1/devices/{device_id}/state/{namespace}` (M12): the merged
/// last-write-wins view across all the account's devices. Only the fields
/// this client consumes are declared; the wire also carries `namespace` and
/// per-entry `device_id`/`updated_at`.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceStateDto {
    #[serde(default)]
    pub entries: HashMap<String, DeviceStateEntryDto>,
}

/// One winning entry in a merged device-state read (M12).
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceStateEntryDto {
    pub value: Value,
}

#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    pub data: T,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

#[derive(Debug, Deserialize)]
pub struct SendResultDto {
    pub event_id: String,
}

/// Optional relation a sent message carries (ADR 0032 M4). `reply_to` makes it a
/// plain reply (`m.in_reply_to`); `thread_root` makes it a thread member
/// (`rel_type: m.thread`). Default (both `None`) is an unrelated message; the
/// server builds the concrete `m.relates_to` from these fields.
#[derive(Debug, Clone, Copy, Default)]
pub struct SendRelation<'a> {
    pub reply_to: Option<&'a str>,
    pub thread_root: Option<&'a str>,
}

/// Response from `POST …/media/uploads` (ADR 0059/0062): the handle a later
/// `send_media` call claims. The server response carries additional metadata
/// (`kind`, `filename`, `content_type`, `size_bytes`, `expires_at`) that
/// axon-tui doesn't currently act on, so only `upload_id` is modeled —
/// `serde` ignores the rest rather than erroring on them.
#[derive(Debug, Clone, Deserialize)]
pub struct StagedUploadDto {
    pub upload_id: Uuid,
}

/// Response from `POST …/verify`: the transaction id for the new flow.
#[derive(Debug, Clone, Deserialize)]
pub struct StartVerifyResponse {
    pub flow_id: String,
}

/// A replayable SAS verification flow as returned by `GET …/verify` and
/// `GET …/verify/{flow_id}`. Mirrors the server's `FlowDto`
/// (`crates/axon-api/src/dto.rs`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FlowDto {
    pub flow_id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub device_id: Option<String>,
    pub stage: FlowStage,
    #[serde(default)]
    pub emoji: Option<Vec<EmojiDto>>,
    #[serde(default)]
    pub decimals: Option<[u16; 3]>,
    #[serde(default)]
    pub cancel_reason: Option<String>,
}

/// The lifecycle stage of a verification flow (server `FlowStageDto`).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowStage {
    Requested,
    Ready,
    KeysExchanged,
    Confirmed,
    Done,
    Cancelled,
}

/// One SAS emoji: the symbol and its short English description.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct EmojiDto {
    pub symbol: String,
    pub description: String,
}

/// `GET /v1/accounts/{id}/devices` (M16): the resolved user plus their devices.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DeviceListDto {
    pub user_id: String,
    pub devices: Vec<DeviceListEntryDto>,
}

/// One Matrix device in [`DeviceListDto`]. Extra wire fields are ignored.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DeviceListEntryDto {
    pub device_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AccountDto {
    pub account_id: Uuid,
    pub user_id: String,
    pub state: AccountState,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub verified: Option<bool>,
    /// Live megolm-backup snapshot (ADR 0098). Orthogonal to `verified`.
    /// Absent on older Axon servers; defaults to the unknown snapshot.
    #[serde(default)]
    pub backup: BackupSnapshot,
}

/// Live megolm-backup observation from `AccountDto.backup`.
/// `recovery_state` is 4S completeness, not "history keys imported."
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct BackupSnapshot {
    #[serde(default)]
    pub exists_on_server: Option<bool>,
    #[serde(default)]
    pub this_device_uploading: bool,
    #[serde(default)]
    pub backup_state: BackupState,
    #[serde(default)]
    pub recovery_state: RecoveryState,
}

/// SDK backup machine state on the wire (ADR 0098).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackupState {
    #[default]
    Unknown,
    Creating,
    Enabling,
    Resuming,
    Enabled,
    Downloading,
    Disabling,
}

impl BackupState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Creating => "creating",
            Self::Enabling => "enabling",
            Self::Resuming => "resuming",
            Self::Enabled => "enabled",
            Self::Downloading => "downloading",
            Self::Disabling => "disabling",
        }
    }
}

/// SDK `RecoveryState` on the wire (ADR 0098). Not "keys recovered."
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryState {
    #[default]
    Unknown,
    Enabled,
    Disabled,
    Incomplete,
}

impl RecoveryState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Incomplete => "incomplete",
        }
    }
}

/// What `POST …/backup/enable` did about megolm backup (ADR 0098).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackupAction {
    Joined,
    Enabled,
    ExportPending,
    Failed,
    AlreadyUploading,
}

/// Flattened enable-backup 200 body: `AccountDto` plus `backup_action`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct EnableBackupResponse {
    #[serde(flatten)]
    pub account: AccountDto,
    pub backup_action: BackupAction,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AccountState {
    Active,
    Deactivated,
    Deleting,
}

/// One room member from `GET …/rooms/{room_id}/members`.
#[derive(Debug, Clone, Deserialize)]
pub struct MemberDto {
    pub user_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoomDto {
    pub account_id: Uuid,
    #[serde(default)]
    pub account_user_id: Option<String>,
    pub room_id: String,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub avatar_url: Option<String>,
    pub canonical_alias: Option<String>,
    pub last_activity_ts: i64,
    pub last_event_id: Option<String>,
}

impl RoomDto {
    pub fn title(&self) -> &str {
        self.name
            .as_deref()
            .or(self.canonical_alias.as_deref())
            .unwrap_or(&self.room_id)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TimelinePage {
    pub events: Vec<EventDto>,
    pub next_cursor: Option<String>,
}

/// One thread root in the `GET …/rooms/{room_id}/threads` response (M8 / ADR
/// 0032 M3): the server-aggregated reply count and the id/timestamp of the most
/// recent member. The latest member's sender and body are not carried here, so
/// the TUI resolves them from the loaded slice when building a thread badge.
#[derive(Debug, Clone, Deserialize)]
pub struct ThreadSummaryDto {
    pub root_event_id: String,
    pub reply_count: i64,
    // `latest_reply_event_id` / `latest_reply_ts` are part of the wire shape but
    // the badge resolves the latest member's sender and body from the loaded
    // slice instead, so serde ignores the extra keys.
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchPage {
    pub results: Vec<SearchResultDto>,
    pub total: usize,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchResultDto {
    pub event: EventDto,
    pub score: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventDto {
    pub account_id: Uuid,
    pub event_id: String,
    pub room_id: String,
    pub sender: String,
    #[serde(default)]
    pub state_key: Option<String>,
    /// The monotonic sequence in which this account ingested events — the row's
    /// `events.id`, independent of `origin_ts`.
    ///
    /// Every read surface sorts by `origin_ts` (display order), and the two
    /// disagree whenever a homeserver delivers an event stamped earlier than
    /// events already held — routinely, for a bridge backfilling a conversation
    /// into a freshly created portal. A Matrix read receipt is interpreted in
    /// arrival order, so marking a room read names the greatest `arrival_order`
    /// among the events actually displayed, not the display-last one
    /// (ADR 0089); see [`crate::app::read_markers`].
    ///
    /// Comparable only within one room of one account, and only for ordering —
    /// the values are not contiguous and carry no other meaning.
    ///
    /// Deliberately **not** `#[serde(default)]`, unlike the optional fields
    /// around it: the server always emits this, and a defaulted `0` is not
    /// inert. It would win the first receipt-target comparison in a session and
    /// refuse every later one, freezing the target on the first event forever —
    /// a quieter version of the bug ADR 0089 fixes. Failing to deserialize
    /// against an axon too old to send it is the correct, loud outcome.
    pub arrival_order: i64,
    pub origin_ts: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub content: Option<Value>,
    pub body: Option<String>,
    pub relates_to: Option<Value>,
    pub redacted: bool,
    pub redaction_event_id: Option<String>,
    /// Server-aggregated per-emoji reaction tally (M8), keyed by reaction key.
    /// The collapsed timeline strips raw `m.reaction` rows, so reaction badges and
    /// the ids needed to withdraw a reaction come from here rather than from
    /// scanning events. `None` on the live `/v1/ws` stream and for events with no
    /// reactions.
    #[serde(default)]
    pub reactions: Option<HashMap<String, ReactionTally>>,
    /// Sender-device trust snapshot at decrypt time (M7c / ADR 0031): one of
    /// `verified`, `unverified`, `unknown`, `verification_violation`. `None` for
    /// unencrypted events or rows with no recorded verdict.
    #[serde(default)]
    pub sender_trust: Option<String>,
}

/// One emoji's aggregated tally on an [`EventDto`], mirroring the API's
/// `ReactionDto`. `my_event_ids` are the account user's own reaction events for
/// the key — the ids redacted to withdraw the reaction.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReactionTally {
    pub count: i64,
    pub me: bool,
    /// The distinct Matrix user ids that reacted with this key.
    ///
    /// Deserialized so a live `m.reaction` frame can tell a sender already in
    /// the tally from a new one and patch it idempotently, rather than waiting
    /// for the next timeline reload to re-derive the aggregate. `count` is this
    /// list's cardinality server-side, but is incremented alongside rather than
    /// recomputed from it: the store caps a pathological event at its oldest
    /// 1000 distinct `(sender, key)` pairs, and a truncated `senders` must not
    /// silently rewrite an authoritative `count`.
    #[serde(default)]
    pub senders: Vec<String>,
    #[serde(default)]
    pub my_event_ids: Vec<String>,
}

#[derive(Clone, Copy)]
enum MediaKind {
    Image,
    File,
    Audio,
    Video,
    Sticker,
}

struct ParsedMedia<'a> {
    kind: MediaKind,
    url: Option<&'a str>,
    filename: &'a str,
    caption: Option<&'a str>,
    encrypted: bool,
}

impl EventDto {
    pub fn display_body(&self) -> String {
        if self.redacted {
            return "[redacted]".to_owned();
        }
        if let Some(membership) = self.membership_change() {
            return match membership.as_str() {
                "join" => format!("{} joined the room", self.sender),
                "leave" => format!("{} left the room", self.sender),
                "ban" => format!("{} was banned from the room", self.sender),
                "invite" => format!("{} was invited to the room", self.sender),
                _ => format!("{} membership changed: {membership}", self.sender),
            };
        }
        // Describe media messages by type + filename instead of falling through
        // to the raw event-type label.
        if let Some(label) = self.media_label() {
            return label;
        }
        if let Some(body) = &self.body {
            return body.clone();
        }
        if self.content.is_none() {
            return "[unable to decrypt]".to_owned();
        }
        format!("[{}]", self.event_type)
    }

    /// A human-readable label for media message types (`m.image`, `m.file`,
    /// `m.audio`, `m.video`, `m.sticker`). Returns `None` for text messages
    /// and non-message events.
    fn media_label(&self) -> Option<String> {
        let media = self.parsed_media()?;
        let kind = match media.kind {
            MediaKind::Image => "image",
            MediaKind::File => "file",
            MediaKind::Audio => "audio",
            MediaKind::Video => "video",
            MediaKind::Sticker => "sticker",
        };
        let suffix = media
            .caption
            .map(|caption| format!("\n{caption}"))
            .unwrap_or_default();
        Some(format!("[{kind}: {}]{suffix}", media.filename))
    }

    /// Returns `true` when this image/sticker event uses encrypted media
    /// (`content.file.url`) rather than a plain `content.url`. The Axon media
    /// proxy attempts server-side decryption, but may not have the key for
    /// older messages — in that case it returns raw ciphertext.
    pub fn image_is_encrypted(&self) -> bool {
        self.image_media().is_some_and(|media| media.encrypted)
    }

    /// Extract the `mxc://` URI and account from an image or sticker event.
    /// Returns `(account_id, mxc_url)` when the event carries a downloadable
    /// image, `None` otherwise.
    pub fn image_mxc(&self) -> Option<(Uuid, String)> {
        let media = self.image_media()?;
        Some((self.account_id, media.url?.to_owned()))
    }

    /// Returns the filename for an image/sticker event (the `filename` field if
    /// present, otherwise `body`). Returns `None` for non-image events.
    pub fn image_filename(&self) -> Option<String> {
        Some(self.image_media()?.filename.to_owned())
    }

    /// Returns the user-authored caption for an image/sticker event — present
    /// only when `filename` and `body` are both set and differ. Returns `None`
    /// for non-image events or images without a caption.
    pub fn image_caption(&self) -> Option<String> {
        self.image_media()?.caption.map(str::to_owned)
    }

    fn parsed_media(&self) -> Option<ParsedMedia<'_>> {
        let content = self.content.as_ref()?;
        let kind = match content.get("msgtype").and_then(|value| value.as_str()) {
            Some("m.image") => MediaKind::Image,
            Some("m.file") => MediaKind::File,
            Some("m.audio") => MediaKind::Audio,
            Some("m.video") => MediaKind::Video,
            _ if self.event_type == "m.sticker" => MediaKind::Sticker,
            _ => return None,
        };
        let explicit_filename = content.get("filename").and_then(|value| value.as_str());
        let body = content.get("body").and_then(|value| value.as_str());
        let filename = explicit_filename.or(body).unwrap_or("media");
        let caption = explicit_filename.and_then(|_| body.filter(|body| *body != filename));
        let plain_url = content.get("url").and_then(|value| value.as_str());
        let encrypted_url = content
            .get("file")
            .and_then(|file| file.get("url"))
            .and_then(|value| value.as_str());
        let url = plain_url.or(encrypted_url);
        Some(ParsedMedia {
            kind,
            url,
            filename,
            caption,
            encrypted: plain_url.is_none() && encrypted_url.is_some(),
        })
    }

    fn image_media(&self) -> Option<ParsedMedia<'_>> {
        let media = self.parsed_media()?;
        if !matches!(media.kind, MediaKind::Image | MediaKind::Sticker) {
            return None;
        }
        if !media.url.is_some_and(|url| url.starts_with("mxc://")) {
            return None;
        }
        Some(media)
    }

    pub fn formatted_body(&self) -> Option<&str> {
        let content = self.content.as_ref()?;
        (content.get("format")?.as_str()? == "org.matrix.custom.html")
            .then(|| content.get("formatted_body")?.as_str())
            .flatten()
    }

    pub fn is_message_event(&self) -> bool {
        matches!(
            self.event_type.as_str(),
            "m.room.message" | "m.room.encrypted" | "m.sticker"
        )
    }

    pub fn is_membership_event(&self) -> bool {
        self.event_type == "m.room.member"
    }

    pub fn membership_change(&self) -> Option<String> {
        (self.event_type == "m.room.member")
            .then(|| {
                self.content
                    .as_ref()
                    .and_then(|content| content.get("membership"))
                    .and_then(|membership| membership.as_str())
                    .map(str::to_owned)
            })
            .flatten()
    }

    pub fn edit_relation(&self) -> Option<(&str, &str, &Value)> {
        let relates_to = self.relates_to.as_ref()?;
        if relates_to.get("rel_type")?.as_str()? != "m.replace" {
            return None;
        }
        let target = relates_to.get("event_id")?.as_str()?;
        let new_content = self.content.as_ref()?.get("m.new_content")?;
        let new_body = new_content.get("body")?.as_str()?;
        Some((target, new_body, new_content))
    }

    /// The `(target event id, key)` an `m.reaction` annotates
    /// (`m.relates_to` with `rel_type: m.annotation`).
    pub fn reaction_relation(&self) -> Option<(&str, &str)> {
        // Matrix permits any event type to carry `m.annotation`, and ADR 0033
        // restricts aggregation to `m.reaction` deliberately: a non-reaction
        // annotation (a `com.example.approval`, say) is tracked separately
        // (#112) and renders as its own row. Matching on `rel_type` alone would
        // fold one into the badge for a key the server's own aggregation never
        // includes, while it still rendered as a row — a double representation
        // that only a reload would clear.
        if self.event_type != "m.reaction" {
            return None;
        }
        let relates_to = self.relates_to.as_ref()?;
        if relates_to.get("rel_type")?.as_str()? != "m.annotation" {
            return None;
        }
        let target = relates_to.get("event_id")?.as_str()?;
        let key = relates_to.get("key")?.as_str()?;
        Some((target, key))
    }

    /// The `event_id` this event is a reply to (`m.relates_to.m.in_reply_to`),
    /// when present and the event is *not* a thread member. A threaded message
    /// may carry a fallback `m.in_reply_to` for older clients (Matrix 1.3); we
    /// surface that through [`thread_relation`](Self::thread_relation) instead so
    /// a threaded reply is not double-processed as a plain reply (ADR 0032 M1).
    pub fn reply_relation(&self) -> Option<&str> {
        let relates_to = self.relates_to.as_ref()?;
        if relates_to.get("rel_type").and_then(Value::as_str) == Some("m.thread") {
            return None;
        }
        relates_to.get("m.in_reply_to")?.get("event_id")?.as_str()
    }

    /// The thread root `event_id` this event belongs to, when it carries
    /// `m.relates_to` with `rel_type: m.thread` (ADR 0032 M2).
    pub fn thread_relation(&self) -> Option<&str> {
        let relates_to = self.relates_to.as_ref()?;
        if relates_to.get("rel_type")?.as_str()? != "m.thread" {
            return None;
        }
        relates_to.get("event_id")?.as_str()
    }

    pub fn state_key(&self) -> Option<&str> {
        self.state_key.as_deref()
    }

    pub fn membership_display_name(&self) -> Option<&str> {
        (self.event_type == "m.room.member")
            .then(|| {
                self.content
                    .as_ref()
                    .and_then(|content| content.get("displayname"))
                    .and_then(|display_name| display_name.as_str())
            })
            .flatten()
    }
}

#[derive(Debug, Deserialize)]
pub struct WsEnvelope<T> {
    #[serde(rename = "type")]
    pub kind: String,
    pub account_id: Uuid,
    pub payload: T,
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("{0}")]
    Request(String),
    #[error("invalid API JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid base URL: {0}")]
    Url(String),
    #[error("unsupported base URL scheme: {0}")]
    UnsupportedScheme(String),
    #[error("HTTP {status}: {message}")]
    Status { status: StatusCode, message: String },
}

impl ApiError {
    /// `true` when this is an HTTP 404. Used to detect a verification flow the
    /// server no longer has a record of (ADR 0028 §3).
    pub fn is_not_found(&self) -> bool {
        matches!(self, ApiError::Status { status, .. } if *status == StatusCode::NOT_FOUND)
    }

    pub fn is_service_unavailable(&self) -> bool {
        matches!(self, ApiError::Status { status, .. } if *status == StatusCode::SERVICE_UNAVAILABLE)
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(err: reqwest::Error) -> Self {
        ApiError::Request(if err.is_timeout() {
            "request timed out".to_owned()
        } else {
            format!("request failed: {err}")
        })
    }
}

fn path_segment(value: &str) -> Escaped<'_> {
    Escaped(value)
}

struct Escaped<'a>(&'a str);

impl fmt::Display for Escaped<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    write!(f, "{}", byte as char)?;
                }
                _ => write!(f, "%{byte:02X}")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_room_response() {
        let body = r##"{
            "data": [{
                "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "account_user_id": "@alice:localhost",
                "room_id": "!room:localhost",
                "name": "Ops",
                "topic": null,
                "avatar_url": "mxc://localhost/avatar",
                "canonical_alias": "#ops:localhost",
                "last_activity_ts": 1234,
                "last_event_id": "$event:localhost"
            }]
        }"##;
        let response: ApiResponse<Vec<RoomDto>> = serde_json::from_str(body).unwrap();
        assert_eq!(response.data[0].title(), "Ops");
        assert_eq!(
            response.data[0].canonical_alias.as_deref(),
            Some("#ops:localhost")
        );
        assert_eq!(
            response.data[0].account_user_id.as_deref(),
            Some("@alice:localhost")
        );
    }

    #[test]
    fn deserializes_room_response_without_account_user_id() {
        let body = r##"{
            "data": [{
                "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "room_id": "!room:localhost",
                "name": "Ops",
                "topic": null,
                "avatar_url": "mxc://localhost/avatar",
                "canonical_alias": "#ops:localhost",
                "last_activity_ts": 1234,
                "last_event_id": "$event:localhost"
            }]
        }"##;
        let response: ApiResponse<Vec<RoomDto>> = serde_json::from_str(body).unwrap();
        assert_eq!(response.data[0].title(), "Ops");
        assert_eq!(response.data[0].account_user_id, None);
    }

    #[test]
    fn deserializes_timeline_response() {
        let body = r#"{
            "data": {
                "events": [{
                    "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    "event_id": "$event:localhost",
                    "room_id": "!room:localhost",
                    "sender": "@alice:localhost",
                    "state_key": null,
                    "origin_ts": 1234,
                    "arrival_order": 1234,
                    "type": "m.room.message",
                    "content": { "msgtype": "m.text", "body": "hello" },
                    "body": "hello",
                    "relates_to": null,
                    "redacted": false,
                    "redaction_event_id": null
                }],
                "next_cursor": "MTAuMQ"
            }
        }"#;
        let response: ApiResponse<TimelinePage> = serde_json::from_str(body).unwrap();
        assert_eq!(response.data.events[0].display_body(), "hello");
        assert_eq!(response.data.events[0].arrival_order, 1234);
        assert_eq!(response.data.next_cursor.as_deref(), Some("MTAuMQ"));
    }

    /// `arrival_order` deliberately carries no `#[serde(default)]`, unlike the
    /// optional fields around it. A defaulted `0` is not inert: it would win the
    /// first receipt-target comparison of a session and refuse every later one,
    /// freezing the receipt on the first event forever and silently (ADR 0089).
    /// Failing loudly against an axon too old to send the field is the correct
    /// outcome — that server cannot support this client.
    #[test]
    fn event_without_arrival_order_is_rejected() {
        let body = r#"{
            "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "event_id": "$event:localhost",
            "room_id": "!room:localhost",
            "sender": "@alice:localhost",
            "state_key": null,
            "origin_ts": 1234,
            "type": "m.room.message",
            "content": { "msgtype": "m.text", "body": "hello" },
            "body": "hello",
            "relates_to": null,
            "redacted": false,
            "redaction_event_id": null
        }"#;
        let err = serde_json::from_str::<EventDto>(body).unwrap_err();
        assert!(
            err.to_string().contains("arrival_order"),
            "expected a missing-field error naming arrival_order, got: {err}"
        );
    }

    #[test]
    fn formatted_body_requires_matrix_html_format() {
        let body = r#"{
            "data": {
                "events": [{
                    "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    "event_id": "$event:localhost",
                    "room_id": "!room:localhost",
                    "sender": "@alice:localhost",
                    "origin_ts": 1234,
                    "arrival_order": 1234,
                    "type": "m.room.message",
                    "content": {
                        "msgtype": "m.text",
                        "body": "hello",
                        "format": "org.matrix.custom.html",
                        "formatted_body": "<strong>hello</strong>"
                    },
                    "body": "hello",
                    "relates_to": null,
                    "redacted": false,
                    "redaction_event_id": null
                }],
                "next_cursor": null
            }
        }"#;
        let response: ApiResponse<TimelinePage> = serde_json::from_str(body).unwrap();
        assert_eq!(
            response.data.events[0].formatted_body(),
            Some("<strong>hello</strong>")
        );
    }

    #[test]
    fn edit_relation_preserves_formatted_new_content() {
        let event: EventDto = serde_json::from_value(serde_json::json!({
            "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "event_id": "$edit:localhost",
            "room_id": "!room:localhost",
            "sender": "@alice:localhost",
            "origin_ts": 1234,
            "arrival_order": 1234,
            "type": "m.room.message",
            "content": {
                "msgtype": "m.text",
                "body": "* hello",
                "m.new_content": {
                    "msgtype": "m.text",
                    "body": "hello",
                    "format": "org.matrix.custom.html",
                    "formatted_body": "<strong>hello</strong>"
                }
            },
            "body": "* hello",
            "relates_to": {
                "rel_type": "m.replace",
                "event_id": "$original:localhost"
            },
            "redacted": false,
            "redaction_event_id": null
        }))
        .expect("valid event");

        let (target, body, new_content) = event.edit_relation().expect("edit relation");
        assert_eq!(target, "$original:localhost");
        assert_eq!(body, "hello");
        assert_eq!(
            new_content.get("formatted_body").and_then(Value::as_str),
            Some("<strong>hello</strong>")
        );
    }

    fn event_with_relation(relates_to: Value) -> EventDto {
        serde_json::from_value(serde_json::json!({
            "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "event_id": "$reply:localhost",
            "room_id": "!room:localhost",
            "sender": "@alice:localhost",
            "origin_ts": 1234,
            "arrival_order": 1234,
            "type": "m.room.message",
            "content": { "msgtype": "m.text", "body": "hi" },
            "body": "hi",
            "relates_to": relates_to,
            "redacted": false,
            "redaction_event_id": null
        }))
        .expect("valid event")
    }

    #[test]
    fn reply_relation_reads_in_reply_to_target() {
        let event = event_with_relation(serde_json::json!({
            "m.in_reply_to": { "event_id": "$original:localhost" }
        }));
        assert_eq!(event.reply_relation(), Some("$original:localhost"));
        assert_eq!(event.thread_relation(), None);
    }

    #[test]
    fn thread_member_is_not_reported_as_a_plain_reply() {
        // A threaded message may carry a fallback m.in_reply_to for older
        // clients; it must surface as a thread relation, not a reply (ADR 0032).
        let event = event_with_relation(serde_json::json!({
            "rel_type": "m.thread",
            "event_id": "$root:localhost",
            "m.in_reply_to": { "event_id": "$latest:localhost" }
        }));
        assert_eq!(event.reply_relation(), None);
        assert_eq!(event.thread_relation(), Some("$root:localhost"));
    }

    #[test]
    fn plain_message_has_no_reply_or_thread_relation() {
        let event = event_with_relation(Value::Null);
        assert_eq!(event.reply_relation(), None);
        assert_eq!(event.thread_relation(), None);
    }

    #[test]
    fn image_accessors_share_encrypted_media_metadata() {
        let event: EventDto = serde_json::from_value(serde_json::json!({
            "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "event_id": "$event:localhost",
            "room_id": "!room:localhost",
            "sender": "@alice:localhost",
            "origin_ts": 1234,
            "arrival_order": 1234,
            "type": "m.room.message",
            "content": {
                "msgtype": "m.image",
                "body": "A caption",
                "filename": "photo.jpg",
                "file": { "url": "mxc://localhost/photo" }
            },
            "body": "A caption",
            "relates_to": null,
            "redacted": false,
            "redaction_event_id": null
        }))
        .unwrap();

        assert_eq!(
            event.image_mxc(),
            Some((
                Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap(),
                "mxc://localhost/photo".to_owned()
            ))
        );
        assert!(event.image_is_encrypted());
        assert_eq!(event.image_filename().as_deref(), Some("photo.jpg"));
        assert_eq!(event.image_caption().as_deref(), Some("A caption"));
        assert_eq!(event.display_body(), "[image: photo.jpg]\nA caption");
    }

    #[test]
    fn deserializes_websocket_frame() {
        let body = r#"{
            "type": "timeline.event",
            "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "payload": {
                "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "event_id": "$event:localhost",
                "room_id": "!room:localhost",
                "sender": "@alice:localhost",
                "state_key": "@alice:localhost",
                "origin_ts": 1234,
                "arrival_order": 1234,
                "type": "m.room.message",
                "content": { "msgtype": "m.text", "body": "hello" },
                "body": "hello",
                "relates_to": null,
                "redacted": false,
                "redaction_event_id": null
            }
        }"#;
        let frame: WsEnvelope<EventDto> = serde_json::from_str(body).unwrap();
        assert_eq!(frame.kind, "timeline.event");
        assert_eq!(frame.payload.room_id, "!room:localhost");
        assert_eq!(frame.payload.state_key(), Some("@alice:localhost"));
    }

    #[test]
    fn demux_routes_timeline_frame() {
        let body = r#"{
            "type": "timeline.event",
            "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "payload": {
                "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "event_id": "$e:localhost", "room_id": "!r:localhost",
                "sender": "@a:localhost", "origin_ts": 1, "arrival_order": 1,
                "type": "m.room.message",
                "content": null, "body": "hi", "relates_to": null,
                "redacted": false, "redaction_event_id": null
            }
        }"#;
        assert!(matches!(
            decode_ws_frame(body),
            Some(LiveFrame::Timeline(_))
        ));
    }

    #[test]
    fn demux_timeline_account_mismatch_is_protocol_error() {
        let body = r#"{
            "type": "timeline.event",
            "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "payload": {
                "account_id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                "event_id": "$e:localhost", "room_id": "!r:localhost",
                "sender": "@a:localhost", "origin_ts": 1, "arrival_order": 1,
                "type": "m.room.message",
                "content": null, "body": "hi", "relates_to": null,
                "redacted": false, "redaction_event_id": null
            }
        }"#;
        assert!(matches!(
            decode_ws_frame(body),
            Some(LiveFrame::ProtocolError(_))
        ));
    }

    #[test]
    fn demux_routes_verification_sas_frame() {
        let body = r#"{
            "type": "verification.sas",
            "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "payload": {
                "flow_id": "txn1", "device_id": "DEV",
                "emoji": [{"symbol": "🐶", "description": "Dog"}],
                "decimals": [1, 2, 3]
            }
        }"#;
        match decode_ws_frame(body) {
            Some(LiveFrame::Verification(frame)) => {
                assert_eq!(frame.kind, VerificationFrameKind::Sas);
                assert_eq!(frame.payload.flow_id, "txn1");
                assert_eq!(frame.payload.decimals, Some([1, 2, 3]));
                assert_eq!(frame.payload.emoji.unwrap()[0].symbol, "🐶");
            }
            other => panic!("expected verification frame, got {other:?}"),
        }
    }

    #[test]
    fn demux_routes_verification_cancelled_frame() {
        let body = r#"{
            "type": "verification.cancelled",
            "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "payload": { "flow_id": "txn1", "device_id": "DEV", "reason": "user" }
        }"#;
        match decode_ws_frame(body) {
            Some(LiveFrame::Verification(frame)) => {
                assert_eq!(frame.kind, VerificationFrameKind::Cancelled);
                assert_eq!(frame.payload.reason.as_deref(), Some("user"));
            }
            other => panic!("expected cancelled frame, got {other:?}"),
        }
    }

    #[test]
    fn demux_routes_sender_trust_violation_frame() {
        let body = r#"{
            "type": "sender_trust.violation",
            "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "payload": { "user_id": "@mallory:localhost", "verification_violation": true }
        }"#;
        match decode_ws_frame(body) {
            Some(LiveFrame::SenderTrustViolation { payload, .. }) => {
                assert_eq!(payload.user_id, "@mallory:localhost");
                assert!(payload.verification_violation);
            }
            other => panic!("expected trust violation frame, got {other:?}"),
        }
    }

    #[test]
    fn demux_routes_ephemeral_passthrough_frame() {
        let body = r#"{
            "type": "ephemeral.passthrough",
            "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "payload": {
                "room_id": "!r:localhost",
                "event_type": "m.typing",
                "content": { "user_ids": ["@bob:localhost"] }
            }
        }"#;
        match decode_ws_frame(body) {
            Some(LiveFrame::Ephemeral { payload, .. }) => {
                assert_eq!(payload.room_id.as_deref(), Some("!r:localhost"));
                assert_eq!(payload.event_type, "m.typing");
                assert_eq!(
                    payload.content,
                    serde_json::json!({ "user_ids": ["@bob:localhost"] })
                );
            }
            other => panic!("expected ephemeral frame, got {other:?}"),
        }
    }

    #[test]
    fn demux_ignores_unknown_frame_kind() {
        let body = r#"{
            "type": "future.frame",
            "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "payload": {}
        }"#;
        assert!(decode_ws_frame(body).is_none());
    }

    #[test]
    fn deserializes_flow_dto_with_snake_case_stage() {
        let body = r#"{
            "data": {
                "flow_id": "txn1", "device_id": "DEV",
                "stage": "keys_exchanged",
                "emoji": [{"symbol": "🐱", "description": "Cat"}],
                "decimals": [4, 5, 6], "cancel_reason": null
            }
        }"#;
        let response: ApiResponse<FlowDto> = serde_json::from_str(body).unwrap();
        assert_eq!(response.data.stage, FlowStage::KeysExchanged);
        assert_eq!(response.data.decimals, Some([4, 5, 6]));
    }

    #[test]
    fn not_found_is_detected() {
        let err = ApiError::Status {
            status: StatusCode::NOT_FOUND,
            message: "gone".to_owned(),
        };
        assert!(err.is_not_found());
        let other = ApiError::Status {
            status: StatusCode::CONFLICT,
            message: "x".to_owned(),
        };
        assert!(!other.is_not_found());
    }

    #[test]
    fn live_reconnect_backoff_doubles_until_cap() {
        assert_eq!(
            next_live_reconnect_backoff(Duration::from_secs(1)),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_live_reconnect_backoff(Duration::from_secs(16)),
            Duration::from_secs(30)
        );
        assert_eq!(
            next_live_reconnect_backoff(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn timeout_buckets_are_correct() {
        let client = AxonClient::new("http://127.0.0.1:8080".to_owned(), None);
        let read = read_request(client.http.get("http://127.0.0.1:8080/v1/accounts"))
            .build()
            .unwrap();
        let mutation = message_mutation(
            client
                .http
                .post("http://127.0.0.1:8080/v1/accounts/id/rooms/room/send"),
        )
        .build()
        .unwrap();
        let lc = lifecycle(client.http.post("http://127.0.0.1:8080/v1/accounts/login"))
            .build()
            .unwrap();
        let media = media_request(
            client
                .http
                .get("http://127.0.0.1:8080/v1/media/id/server/media"),
        )
        .build()
        .unwrap();

        assert_eq!(read.timeout(), Some(&HTTP_REQUEST_TIMEOUT));
        assert_eq!(mutation.timeout(), Some(&MESSAGE_MUTATION_TIMEOUT));
        assert_eq!(
            lc.timeout(),
            Some(&LIFECYCLE_TIMEOUT),
            "lifecycle ops need a generous timeout to survive megolm key import"
        );
        assert_eq!(
            media.timeout(),
            Some(&MEDIA_TIMEOUT),
            "stalled media responses must release the bounded worker pool"
        );
    }

    #[test]
    fn escapes_path_segments() {
        assert_eq!(
            path_segment("$event:local/host").to_string(),
            "%24event%3Alocal%2Fhost"
        );
    }

    #[tokio::test]
    async fn rejects_empty_mxc_server_or_media_id() {
        let client = AxonClient::new("http://127.0.0.1:8080".to_owned(), None);

        assert!(matches!(
            client.get_media(Uuid::nil(), "mxc:///media").await,
            Err(ApiError::Url(_))
        ));
        assert!(matches!(
            client.get_media(Uuid::nil(), "mxc://server/").await,
            Err(ApiError::Url(_))
        ));
    }

    #[test]
    fn deserializes_account_response() {
        let body = r#"{
            "data": {
                "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "user_id": "@alice:example.com",
                "homeserver_url": "https://example.com",
                "device_id": "DEVICE",
                "state": "active",
                "verified": null,
                "created_at": "2026-06-10T00:00:00Z",
                "updated_at": "2026-06-10T00:00:00Z"
            }
        }"#;
        let response: ApiResponse<AccountDto> = serde_json::from_str(body).unwrap();
        assert_eq!(response.data.user_id, "@alice:example.com");
        assert_eq!(response.data.state, AccountState::Active);
        assert_eq!(response.data.device_id.as_deref(), Some("DEVICE"));
        assert_eq!(response.data.backup, BackupSnapshot::default());
    }

    #[test]
    fn deserializes_device_list_response() {
        let body = r#"{
            "data": {
                "user_id": "@alice:example.com",
                "devices": [
                    {
                        "device_id": "AXONDEV",
                        "display_name": "axon",
                        "is_verified": true,
                        "is_cross_signed_by_owner": true,
                        "local_trust_state": "verified",
                        "algorithms": []
                    }
                ]
            }
        }"#;
        let response: ApiResponse<DeviceListDto> = serde_json::from_str(body).unwrap();
        assert_eq!(response.data.user_id, "@alice:example.com");
        assert_eq!(response.data.devices[0].device_id, "AXONDEV");
        assert_eq!(
            response.data.devices[0].display_name.as_deref(),
            Some("axon")
        );
    }

    #[test]
    fn deserializes_enable_backup_response_flatten() {
        let body = r#"{
            "data": {
                "account_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "user_id": "@alice:example.com",
                "homeserver_url": "https://example.com",
                "device_id": "DEVICE",
                "state": "active",
                "verified": true,
                "backup": {
                    "exists_on_server": true,
                    "this_device_uploading": true,
                    "backup_state": "enabled",
                    "recovery_state": "enabled"
                },
                "backup_action": "enabled",
                "created_at": "2026-06-10T00:00:00Z",
                "updated_at": "2026-06-10T00:00:00Z"
            }
        }"#;
        let response: ApiResponse<EnableBackupResponse> = serde_json::from_str(body).unwrap();
        assert_eq!(response.data.account.user_id, "@alice:example.com");
        assert_eq!(response.data.backup_action, BackupAction::Enabled);
        assert_eq!(response.data.account.backup.exists_on_server, Some(true));
        assert!(response.data.account.backup.this_device_uploading);
        assert_eq!(
            response.data.account.backup.backup_state,
            BackupState::Enabled
        );
        assert_eq!(
            response.data.account.backup.recovery_state,
            RecoveryState::Enabled
        );
    }

    #[test]
    fn deserializes_staged_upload_response_ignoring_extra_fields() {
        // The real server response also carries kind/filename/content_type/
        // size_bytes/expires_at; StagedUploadDto only models upload_id, so
        // this proves the unmodeled fields are tolerated, not rejected.
        let body = r#"{
            "data": {
                "upload_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "kind": "image",
                "filename": "photo.png",
                "content_type": "image/png",
                "size_bytes": 1234,
                "expires_at": "2026-07-11T01:00:00Z"
            }
        }"#;
        let response: ApiResponse<StagedUploadDto> = serde_json::from_str(body).unwrap();
        assert_eq!(
            response.data.upload_id,
            Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap()
        );
    }
}
