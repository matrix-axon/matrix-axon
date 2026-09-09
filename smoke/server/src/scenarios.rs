use std::time::Duration;

use anyhow::{anyhow, bail};
use serde_json::Value;

use crate::runner::{Ctx, ScenarioOutcome};
use crate::util::{stays_true, wait_for};
use crate::wire::{EventDto, RoomDto};

/// How long [`unread_counts`] watches `notification_count` for stability after
/// the account's own message reaches the timeline, before concluding the
/// server-side unread-counts watcher (an independent async task, with no
/// ordering guarantee relative to timeline persistence) didn't react to it.
const UNREAD_COUNTS_OWN_MESSAGE_SETTLE: Duration = Duration::from_secs(3);

/// Wait until the seeded room projections required by the server scenarios are
/// available.
///
/// Local-stack's startup probe intentionally waits for one room, which is
/// enough for an interactive client to connect but does not order the other
/// room projections behind it.
/// Server smoke needs all of its fixtures before its first assertion: the
/// room-list scenario reads every room, `timeline_read` reads the general
/// timeline, and relation scenarios require the edited relations root.
pub async fn seeded_fixtures_ready(ctx: &Ctx) -> anyhow::Result<()> {
    wait_for("seeded server fixtures to project", ctx.timeout, || async {
        let rooms = ctx.axon.list_rooms().await?;
        let all_rooms_present = [
            &ctx.manifest.rooms.general.room_id,
            &ctx.manifest.rooms.long_timeline.room_id,
            &ctx.manifest.rooms.relations.room_id,
        ]
        .into_iter()
        .all(|room_id| rooms.iter().any(|room| room.room_id == *room_id));
        if !all_rooms_present {
            return Ok(false);
        }

        let account_id = ctx.manifest.axon_account_id;
        let general = ctx
            .axon
            .timeline(account_id, &ctx.manifest.rooms.general.room_id, 20)
            .await?;
        let relations = ctx
            .axon
            .timeline(account_id, &ctx.manifest.rooms.relations.room_id, 20)
            .await?;

        Ok(general.events.iter().any(|event| {
            event
                .body
                .as_deref()
                .is_some_and(|body| body.contains("general"))
        }) && relations
            .events
            .iter()
            .any(|event| event.body.as_deref() == Some("relations root edited")))
    })
    .await
}

pub async fn boot_health(ctx: &Ctx) -> ScenarioOutcome {
    ScenarioOutcome::from_result(
        async {
            let health = ctx.axon.health().await?;
            if health.get("status").and_then(Value::as_str) != Some("ok") {
                bail!("unexpected /healthz response: {health}");
            }
            Ok(())
        }
        .await,
    )
}

pub async fn account_visible(ctx: &Ctx) -> ScenarioOutcome {
    ScenarioOutcome::from_result(
        async {
            let accounts = ctx.axon.list_accounts().await?;
            let account = accounts
                .iter()
                .find(|account| account.account_id == ctx.manifest.axon_account_id)
                .ok_or_else(|| anyhow!("manifest account not found in GET /v1/accounts"))?;
            if account.user_id != ctx.manifest.accounts.target.user_id {
                bail!("unexpected account user_id {}", account.user_id);
            }
            if account.homeserver_url != ctx.manifest.homeserver_url {
                bail!("unexpected homeserver_url {}", account.homeserver_url);
            }
            if account.state != "active" {
                bail!("expected active account, got {}", account.state);
            }
            if account.device_id.as_deref().unwrap_or_default().is_empty() {
                bail!("active account had no device_id");
            }
            Ok(())
        }
        .await,
    )
}

pub async fn room_list(ctx: &Ctx) -> ScenarioOutcome {
    ScenarioOutcome::from_result(
        async {
            let rooms = ctx.axon.list_rooms().await?;
            require_room(&rooms, &ctx.manifest.rooms.general.room_id, "Smoke General")?;
            require_room(
                &rooms,
                &ctx.manifest.rooms.long_timeline.room_id,
                "Smoke Timeline",
            )?;
            require_room(
                &rooms,
                &ctx.manifest.rooms.relations.room_id,
                "Smoke Relations",
            )?;
            Ok(())
        }
        .await,
    )
}

pub async fn timeline_read(ctx: &Ctx) -> ScenarioOutcome {
    ScenarioOutcome::from_result(
        async {
            let page = ctx
                .axon
                .timeline(
                    ctx.manifest.axon_account_id,
                    &ctx.manifest.rooms.general.room_id,
                    20,
                )
                .await?;
            if !page.events.iter().any(|event| {
                event
                    .body
                    .as_deref()
                    .is_some_and(|body| body.contains("general"))
            }) {
                bail!("general timeline did not contain seeded general message");
            }
            let event = page
                .events
                .iter()
                .find(|event| event.body.is_some())
                .ok_or_else(|| anyhow!("general timeline had no message event"))?;
            let fetched = ctx
                .axon
                .event(ctx.manifest.axon_account_id, &event.event_id)
                .await?;
            if fetched.event_id != event.event_id || fetched.room_id != event.room_id {
                bail!("event lookup did not match timeline event");
            }
            Ok(())
        }
        .await,
    )
}

pub async fn inbound_timeline_ws(ctx: &Ctx) -> ScenarioOutcome {
    ScenarioOutcome::from_result(
        async {
            let peer_token = ctx.matrix.login(&ctx.manifest.accounts.peer).await?;
            let body = format!("server-smoke inbound {}", &ctx.run_id[..8]);
            let txn_id = format!("server-smoke-inbound-{}", &ctx.run_id[..8]);
            let axon = ctx.axon.clone();
            let account_id = ctx.manifest.axon_account_id;
            let ws_body = body.clone();
            let ws_task = tokio::spawn(async move {
                axon.read_ws_until(Duration::from_secs(60), |frame| {
                    if frame.kind != "timeline.event" || frame.account_id != account_id {
                        return false;
                    }
                    frame
                        .payload
                        .get("body")
                        .and_then(Value::as_str)
                        .is_some_and(|seen| seen == ws_body)
                })
                .await
            });
            tokio::time::sleep(Duration::from_millis(300)).await;
            let event_id = ctx
                .matrix
                .send_message(
                    &peer_token,
                    &ctx.manifest.rooms.general.room_id,
                    &txn_id,
                    &body,
                )
                .await?;
            let frames = ws_task.await??;
            ctx.record_ws_frames(frames);
            wait_for("inbound message in Axon timeline", ctx.timeout, || async {
                let page = ctx
                    .axon
                    .timeline(
                        ctx.manifest.axon_account_id,
                        &ctx.manifest.rooms.general.room_id,
                        30,
                    )
                    .await?;
                Ok(page.events.iter().any(|event| {
                    event.event_id == event_id && event.body.as_deref() == Some(&body)
                }))
            })
            .await?;
            Ok(())
        }
        .await,
    )
}

pub async fn outbound_send(ctx: &Ctx) -> ScenarioOutcome {
    ScenarioOutcome::from_result(
        async {
            let peer_token = ctx.matrix.login(&ctx.manifest.accounts.peer).await?;
            let body = format!("server-smoke outbound {}", &ctx.run_id[..8]);
            let send = ctx
                .axon
                .send_message(
                    ctx.manifest.axon_account_id,
                    &ctx.manifest.rooms.general.room_id,
                    &body,
                )
                .await?;
            if send.event_id.is_empty() {
                bail!("send response event_id was empty");
            }
            ctx.matrix
                .wait_for_event(
                    &peer_token,
                    &ctx.manifest.rooms.general.room_id,
                    &send.event_id,
                    &body,
                    ctx.timeout,
                )
                .await?;
            wait_for("outbound message in Axon timeline", ctx.timeout, || async {
                let page = ctx
                    .axon
                    .timeline(
                        ctx.manifest.axon_account_id,
                        &ctx.manifest.rooms.general.room_id,
                        30,
                    )
                    .await?;
                Ok(page.events.iter().any(|event| {
                    event.event_id == send.event_id && event.body.as_deref() == Some(&body)
                }))
            })
            .await?;
            Ok(())
        }
        .await,
    )
}

/// Server-derived unread counts (issue #313, ADR 0070), validated against a
/// real Synapse: a peer's message increases `notification_count` without
/// Axon polling for it or sending anything; marking the room read (ADR 0067's
/// real receipt round trip) clears it back to `0`, proving the count is
/// homeserver-derived rather than an Axon-local tally; and the account's own
/// message never increments it. Runs against `Smoke General`, which is
/// unencrypted — ADR 0070's documented encrypted-room over-counting caveat is
/// deliberately not asserted here, since this smoke room doesn't exercise it.
pub async fn unread_counts(ctx: &Ctx) -> ScenarioOutcome {
    ScenarioOutcome::from_result(
        async {
            let account_id = ctx.manifest.axon_account_id;
            let room_id = ctx.manifest.rooms.general.room_id.clone();

            let baseline = find_room(&ctx.axon.list_rooms().await?, &room_id)?;

            // A peer message is a real, homeserver-derived notification —
            // Axon never sent it and never polled the homeserver for it.
            let peer_token = ctx.matrix.login(&ctx.manifest.accounts.peer).await?;
            let body = format!("server-smoke unread {}", &ctx.run_id[..8]);
            let txn_id = format!("server-smoke-unread-{}", &ctx.run_id[..8]);
            let event_id = ctx
                .matrix
                .send_message(&peer_token, &room_id, &txn_id, &body)
                .await?;
            ctx.matrix
                .wait_for_event(&peer_token, &room_id, &event_id, &body, ctx.timeout)
                .await?;
            wait_for(
                "unread notification_count increases after peer message",
                ctx.timeout,
                || async {
                    let room = find_room(&ctx.axon.list_rooms().await?, &room_id)?;
                    Ok(room.notification_count > baseline.notification_count)
                },
            )
            .await?;

            // Marking the room read round-trips through Synapse's own receipt
            // handling (ADR 0067) — this is what proves the count is
            // homeserver-derived rather than an Axon-local tally.
            ctx.axon.mark_read(account_id, &room_id, &event_id).await?;
            wait_for(
                "unread notification_count clears after marking read",
                ctx.timeout,
                || async {
                    let room = find_room(&ctx.axon.list_rooms().await?, &room_id)?;
                    Ok(room.notification_count == 0)
                },
            )
            .await?;
            let after_read = find_room(&ctx.axon.list_rooms().await?, &room_id)?;

            // Our own message must never increment the count.
            let own_body = format!("server-smoke unread own {}", &ctx.run_id[..8]);
            let own_send = ctx
                .axon
                .send_message(account_id, &room_id, &own_body)
                .await?;
            wait_for(
                "own message reaches the Axon timeline",
                ctx.timeout,
                || async {
                    let page = ctx.axon.timeline(account_id, &room_id, 5).await?;
                    Ok(page
                        .events
                        .iter()
                        .any(|event| event.event_id == own_send.event_id))
                },
            )
            .await?;
            // The timeline write above only proves `persist_timeline_event` ran;
            // it says nothing about the independent `watch_unread_counts` task,
            // which reacts to its own separate `room_info_notable_update_receiver`
            // broadcast with no ordering guarantee relative to timeline persist.
            // Poll for a settle window rather than checking once immediately, so
            // a delayed (not just immediate) regression can't slip past this.
            stays_true(
                "own message never increments notification_count",
                UNREAD_COUNTS_OWN_MESSAGE_SETTLE,
                || async {
                    let room = find_room(&ctx.axon.list_rooms().await?, &room_id)?;
                    Ok(room.notification_count == after_read.notification_count)
                },
            )
            .await?;
            Ok(())
        }
        .await,
    )
}

pub async fn relation_reads(ctx: &Ctx) -> ScenarioOutcome {
    ScenarioOutcome::from_result(
        async {
            let account_id = ctx.manifest.axon_account_id;
            let relations_room = &ctx.manifest.rooms.relations.room_id;
            let fixtures = &ctx.manifest.fixtures.relations;

            let root = ctx.axon.event(account_id, &fixtures.root_event_id).await?;
            if root.body.as_deref() != Some("relations root edited") {
                bail!("edited root body was not collapsed: {:?}", root.body);
            }
            if !root.edited || root.edit_count < 1 {
                bail!("edited root missing edit metadata");
            }
            require_reaction(&root, "👍", false)?;
            require_reaction(&root, "✅", true)?;

            let replies = ctx
                .axon
                .replies(account_id, &fixtures.root_event_id)
                .await?;
            if !replies.iter().any(|event| {
                event.event_id == fixtures.reply_event_id
                    && event.body.as_deref() == Some("relations reply message")
            }) {
                bail!("reply fixture missing from replies endpoint");
            }

            let edits = ctx.axon.edits(account_id, &fixtures.root_event_id).await?;
            if !edits
                .iter()
                .any(|event| event.event_id == fixtures.edit_event_id)
            {
                bail!("edit fixture missing from edits endpoint");
            }

            let reaction_map = ctx
                .axon
                .reactions(account_id, &fixtures.root_event_id)
                .await?;
            if reaction_map.get("👍").is_none() || reaction_map.get("✅").is_none() {
                bail!("reaction endpoint missing seeded reactions: {reaction_map}");
            }

            let redacted = ctx
                .axon
                .event(account_id, &fixtures.redacted_event_id)
                .await?;
            if !redacted.redacted
                || redacted.redaction_event_id.as_deref() != Some(&fixtures.redaction_event_id)
                || redacted.body.is_some()
            {
                bail!("redacted fixture did not render as redacted");
            }

            let threads = ctx.axon.threads(account_id, relations_room).await?;
            if !threads.iter().any(|thread| {
                thread.root_event_id == fixtures.thread_root_event_id && thread.reply_count >= 1
            }) {
                bail!("thread summary missing seeded thread");
            }
            let thread_page = ctx
                .axon
                .thread_timeline(account_id, relations_room, &fixtures.thread_root_event_id)
                .await?;
            if !thread_page
                .events
                .iter()
                .any(|event| event.event_id == fixtures.thread_member_event_id)
            {
                bail!("thread timeline missing seeded thread member");
            }
            Ok(())
        }
        .await,
    )
}

/// Regression coverage for issue #266: a media send with `thread_root` and no
/// `reply_to` must land on the wire as a thread member (`rel_type: m.thread`,
/// no `m.in_reply_to`), not the SDK reply-fallback shape other clients would
/// render as a reply to the thread root. Exercises the manual upload path in
/// `SdkGateway::send_thread_member_attachment` end to end against a real
/// Synapse, reusing the thread already seeded in `Smoke Relations`. The raw
/// event content is fetched straight from the homeserver (not through Axon's
/// own event DTO, which only summarizes relations) so the assertion is on the
/// actual wire shape.
pub async fn media_thread_member(ctx: &Ctx) -> ScenarioOutcome {
    ScenarioOutcome::from_result(
        async {
            let account_id = ctx.manifest.axon_account_id;
            let room_id = ctx.manifest.rooms.relations.room_id.clone();
            let thread_root = ctx.manifest.fixtures.relations.thread_root_event_id.clone();

            let bytes =
                format!("server-smoke media thread member {}", &ctx.run_id[..8]).into_bytes();
            let upload = ctx
                .axon
                .stage_upload(account_id, "file", "thread-note.txt", "text/plain", bytes)
                .await?;
            let send = ctx
                .axon
                .send_media(
                    account_id,
                    &room_id,
                    upload.upload_id,
                    Some(&thread_root),
                    None,
                )
                .await?;
            if send.event_id.is_empty() {
                bail!("send-media response event_id was empty");
            }

            let peer_token = ctx.matrix.login(&ctx.manifest.accounts.peer).await?;
            let content = ctx
                .matrix
                .get_event_content(&peer_token, &room_id, &send.event_id)
                .await?;

            if content.get("msgtype").and_then(Value::as_str) != Some("m.file") {
                bail!("media event had unexpected msgtype: {content}");
            }
            let relates_to = content
                .get("m.relates_to")
                .ok_or_else(|| anyhow!("media event missing m.relates_to: {content}"))?;
            if relates_to.get("rel_type").and_then(Value::as_str) != Some("m.thread") {
                bail!("media event m.relates_to was not a thread relation: {relates_to}");
            }
            if relates_to.get("event_id").and_then(Value::as_str) != Some(thread_root.as_str()) {
                bail!("media event thread relation pointed at the wrong root: {relates_to}");
            }
            if relates_to.get("m.in_reply_to").is_some() {
                bail!(
                    "media event carried an m.in_reply_to fallback — other clients would \
                     render it as a reply to the thread root (#266): {relates_to}"
                );
            }

            wait_for(
                "media thread member reaches the thread timeline",
                ctx.timeout,
                || async {
                    let page = ctx
                        .axon
                        .thread_timeline(account_id, &room_id, &thread_root)
                        .await?;
                    Ok(page
                        .events
                        .iter()
                        .any(|event| event.event_id == send.event_id))
                },
            )
            .await?;
            Ok(())
        }
        .await,
    )
}

pub async fn graceful_stack_shutdown(ctx: &Ctx) -> ScenarioOutcome {
    ScenarioOutcome::from_result(
        async {
            // Actual teardown runs unconditionally in the runner after this loop.
            // Here we assert the pre-teardown invariant that matters for safety:
            // the stack we've been talking to is still up and serving. In attach
            // mode this is the guarantee that the harness has not torn down a
            // stack it does not own.
            let health = ctx.axon.health().await?;
            if health.get("status").and_then(Value::as_str) != Some("ok") {
                bail!("stack unexpectedly unhealthy before teardown: {health}");
            }
            if !ctx.owns_stack {
                eprintln!(
                    "smoke(server): graceful_stack_shutdown attach-mode: stack still up, teardown left to owner"
                );
            }
            Ok(())
        }
        .await,
    )
}

fn find_room(rooms: &[RoomDto], room_id: &str) -> anyhow::Result<RoomDto> {
    rooms
        .iter()
        .find(|room| room.room_id == room_id)
        .cloned()
        .ok_or_else(|| anyhow!("room {room_id} not found"))
}

fn require_room(rooms: &[RoomDto], room_id: &str, name: &str) -> anyhow::Result<()> {
    let room = find_room(rooms, room_id)?;
    if room.name.as_deref() != Some(name) {
        bail!("room {room_id} had unexpected name {:?}", room.name);
    }
    Ok(())
}

fn require_reaction(event: &EventDto, key: &str, me: bool) -> anyhow::Result<()> {
    let reaction = event
        .reactions
        .as_ref()
        .and_then(|map| map.get(key))
        .ok_or_else(|| anyhow!("event {} missing reaction {key}", event.event_id))?;
    if reaction.count < 1 {
        bail!("reaction {key} had count {}", reaction.count);
    }
    if reaction.me != me {
        bail!("reaction {key} me flag was {}, expected {me}", reaction.me);
    }
    Ok(())
}
