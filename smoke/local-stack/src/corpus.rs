//! Render a declarative demo corpus into a live Synapse (ADR 0086).
//!
//! `corpus/demo.toml` is the source of truth for demo content; this module is
//! its only interpreter. Everything it creates goes through the same
//! primitives the older `seed_*` fixtures use — in particular the appservice
//! `?user_id=&ts=` route, which is what lets a message, an image, a reaction or
//! a redaction claim a timestamp from last month.
//!
//! Two things here are load-bearing and easy to break:
//!
//! - **Timestamps are relative.** `at = "-6d 09:12"` is six days before *this
//!   run*, so a recording made at any time reads as current. Seconds are
//!   significant: the gallery run in `#trip-photos` is grouped by ADR 0081's
//!   60-second adjacency heuristic, and the near-miss image four minutes later
//!   must stay out of it.
//! - **Events are sent in timestamp order**, which is also dependency order —
//!   a reply, a thread member, an edit and a redaction all name something
//!   older than themselves.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Duration, NaiveTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::{
    add_space_child, create_dm, create_space, login_matrix, redact_appservice_event,
    register_appservice_user, register_user, send_appservice_event, set_direct_account_data,
    set_membership, set_profile, set_room_avatar, set_room_name, set_room_topic, upload_media,
    MatrixAccount, RoomResponse, SERVER_NAME,
};

/// Reaction, edit and redaction offsets from the message they attach to. They
/// only need to land after their target and before the next thing that would
/// look odd beside them; nothing in the corpus is dense enough for these to
/// collide.
const REACTION_OFFSET_MS: i64 = 5_000;
const EDIT_OFFSET_MS: i64 = 45_000;
const REDACTION_OFFSET_MS: i64 = 120_000;

/// How far ahead of a room's first message its membership and state are dated.
/// An hour reads as "the room was set up, then people talked in it" and keeps
/// setup off the same day separator as the first message.
const SETUP_LEAD_MS: i64 = 3_600_000;

// ---------------------------------------------------------------------------
// The corpus file
// ---------------------------------------------------------------------------

/// `deny_unknown_fields` throughout: a corpus is hand-edited data, and a typo
/// that silently drops a `caption` or a `thread` would surface as a demo that
/// quietly lost its point rather than as an error.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    meta: Meta,
    #[serde(default)]
    persona: Vec<Persona>,
    #[serde(default)]
    space: Vec<Space>,
    #[serde(default)]
    room: Vec<Room>,
    #[serde(default)]
    message: Vec<Message>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Meta {
    name: String,
    #[serde(default)]
    description: String,
    /// The persona whose client the demo is recorded from; axon logs in as this
    /// account, so it is the one persona that needs a password.
    viewer: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Persona {
    id: String,
    localpart: String,
    display_name: String,
    #[serde(default)]
    avatar: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Space {
    id: String,
    name: String,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    avatar: Option<String>,
    #[serde(default)]
    children: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Room {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    avatar: Option<String>,
    /// A DM: no name, no topic, and `m.direct` account data on the viewer.
    #[serde(default)]
    direct: bool,
    members: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    room: String,
    from: String,
    at: String,
    /// Stable handle other messages reference; not a Matrix event ID.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    thread: Option<String>,
    #[serde(default)]
    edited_to: Option<String>,
    #[serde(default)]
    redacted: bool,
    #[serde(default)]
    reactions: Vec<Reaction>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reaction {
    from: String,
    key: String,
}

// ---------------------------------------------------------------------------
// The manifest the run publishes
// ---------------------------------------------------------------------------

/// What a demo driver or a future smoke scenario needs to address the seeded
/// world by corpus name rather than by scraping the screen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemoManifest {
    pub corpus_path: PathBuf,
    pub name: String,
    pub description: String,
    /// The instant every relative `at = "-6d 09:12"` was resolved against.
    ///
    /// Published because the corpus's dates are only meaningful relative to it,
    /// and a stack is designed to outlive the run that seeded it: `demo-stack.sh
    /// up` can leave it running for a recording made on a later calendar day. A
    /// driver that wants the date of some seeded message has to count back from
    /// *this*, not from its own `now`.
    pub seeded_at: DateTime<Utc>,
    /// The account axon is logged in as — the demo's point of view.
    pub viewer: DemoViewer,
    pub personas: BTreeMap<String, DemoPersona>,
    pub spaces: BTreeMap<String, DemoSpace>,
    pub rooms: BTreeMap<String, DemoRoom>,
    /// Corpus `id` → Matrix event ID, for messages that declare one.
    pub events: BTreeMap<String, String>,
    /// Corpus media path → `mxc://` URI.
    pub media: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemoViewer {
    pub persona: String,
    pub account: MatrixAccount,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemoPersona {
    pub user_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemoSpace {
    pub room_id: String,
    pub name: String,
    /// Corpus room ids, in corpus order.
    pub children: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemoRoom {
    pub room_id: String,
    pub name: Option<String>,
    pub direct: bool,
    pub members: Vec<String>,
}

/// The seeded world, plus what `up` needs to point axon at it.
pub struct SeededCorpus {
    pub viewer: MatrixAccount,
    /// A room to wait for in `/v1/rooms` before declaring the stack ready.
    pub sync_probe_room: String,
    pub manifest: DemoManifest,
}

// ---------------------------------------------------------------------------
// Seeding
// ---------------------------------------------------------------------------

struct Media {
    mxc: String,
    mimetype: String,
    size: usize,
    dimensions: Option<(u32, u32)>,
}

/// Read `path`, create everything it declares, and return the addresses of what
/// was created.
pub async fn seed(
    client: &Client,
    hs: &str,
    compose_project: &str,
    path: &Path,
    short: &str,
) -> anyhow::Result<SeededCorpus> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("resolve corpus path {}", path.display()))?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read corpus {}", path.display()))?;
    let corpus: Corpus =
        toml::from_str(&text).with_context(|| format!("parse corpus {}", path.display()))?;
    let media_root = path
        .parent()
        .ok_or_else(|| anyhow!("corpus path has no parent directory"))?
        .to_path_buf();
    // One instant for both, so validation and seeding resolve every relative
    // offset against the same "now".
    let now = Utc::now();
    validate(&corpus, now)?;

    let personas: HashMap<&str, &Persona> = corpus
        .persona
        .iter()
        .map(|p| (p.id.as_str(), p))
        .collect::<HashMap<_, _>>();
    let user_id = |id: &str| -> anyhow::Result<String> {
        personas
            .get(id)
            .map(|p| format!("@{}:{SERVER_NAME}", p.localpart))
            .ok_or_else(|| anyhow!("corpus references unknown persona {id}"))
    };

    // The viewer is a password account: axon logs in with a password, and the
    // appservice can still impersonate it because the wildcard user namespace
    // is non-exclusive (ADR 0086).
    let viewer_persona = personas
        .get(corpus.meta.viewer.as_str())
        .copied()
        .ok_or_else(|| anyhow!("corpus viewer {} is not a persona", corpus.meta.viewer))?;
    let viewer = MatrixAccount {
        user_id: format!("@{}:{SERVER_NAME}", viewer_persona.localpart),
        password: format!("demo-{}-{short}", viewer_persona.localpart),
    };
    register_user(compose_project, &viewer)?;
    let viewer_token = login_matrix(client, hs, &viewer).await?;

    for persona in &corpus.persona {
        if persona.id == viewer_persona.id {
            continue;
        }
        register_appservice_user(client, hs, &user_id(&persona.id)?).await?;
    }

    // Media first: profiles and room avatars all need an mxc before the member
    // events that carry them are written.
    let mut media: HashMap<String, Media> = HashMap::new();
    let mut media_paths: Vec<String> = corpus
        .persona
        .iter()
        .filter_map(|p| p.avatar.clone())
        .collect();
    media_paths.extend(corpus.space.iter().filter_map(|s| s.avatar.clone()));
    media_paths.extend(corpus.room.iter().filter_map(|r| r.avatar.clone()));
    media_paths.extend(corpus.message.iter().filter_map(|m| m.image.clone()));
    for rel in media_paths {
        if media.contains_key(&rel) {
            continue;
        }
        let uploaded = upload_corpus_media(client, hs, &viewer_token, &media_root, &rel).await?;
        media.insert(rel, uploaded);
    }

    for persona in &corpus.persona {
        let mxc = persona
            .avatar
            .as_ref()
            .map(|rel| uploaded_mxc(&media, rel))
            .transpose()?;
        set_profile(
            client,
            hs,
            &user_id(&persona.id)?,
            &persona.display_name,
            mxc.as_deref(),
        )
        .await?;
    }

    // Messages, oldest first — which is also the order relations depend on, and
    // which tells each room when its setup should claim to have happened.
    let ordered = ordered_messages(&corpus, now)?;
    let mut setup_ts: HashMap<&str, i64> = HashMap::new();
    for (ts, message) in &ordered {
        setup_ts
            .entry(message.room.as_str())
            .or_insert(ts - SETUP_LEAD_MS);
    }
    let now_ms = now.timestamp_millis();

    // Rooms. The viewer creates every room, so it holds PL100 everywhere and
    // the demo can exercise anything that needs power; the other personas are
    // invited and join through the appservice.
    //
    // Everything after `/createRoom` is backdated to just before the room's
    // first message, because both clients render membership and state changes
    // as timeline rows. Room creation itself cannot be backdated, so each room
    // opens with a handful of today-dated protocol events — at the *top* of its
    // history, which is the one place a demo does not linger.
    let mut rooms: BTreeMap<String, DemoRoom> = BTreeMap::new();
    let mut direct: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for room in &corpus.room {
        if !room.members.iter().any(|m| m == &viewer_persona.id) {
            bail!(
                "corpus room {} does not include the viewer {}; axon would never see it",
                room.id,
                viewer_persona.id
            );
        }
        let others: Vec<String> = room
            .members
            .iter()
            .filter(|m| *m != &viewer_persona.id)
            .map(|m| user_id(m))
            .collect::<anyhow::Result<_>>()?;
        let ts = *setup_ts
            .get(room.id.as_str())
            .unwrap_or(&(now_ms - SETUP_LEAD_MS));

        let (room_id, is_direct) = if room.direct {
            let peer = match others.as_slice() {
                [peer] => peer.clone(),
                _ => bail!("corpus DM {} must have exactly two members", room.id),
            };
            let room_id = create_dm(client, hs, &viewer_token).await?;
            direct.entry(peer).or_default().push(room_id.clone());
            (room_id, true)
        } else {
            (create_named_room(client, hs, &viewer_token).await?, false)
        };

        for peer in &others {
            let mut invite = serde_json::json!({ "membership": "invite" });
            if is_direct {
                invite["is_direct"] = serde_json::json!(true);
            }
            set_membership(client, hs, &room_id, &viewer.user_id, peer, invite, ts).await?;
            set_membership(
                client,
                hs,
                &room_id,
                peer,
                peer,
                serde_json::json!({ "membership": "join" }),
                ts + 1_000,
            )
            .await?;
        }

        if let Some(name) = &room.name {
            set_room_name(client, hs, &room_id, &viewer.user_id, name, ts + 2_000).await?;
        }
        if let Some(topic) = &room.topic {
            set_room_topic(client, hs, &room_id, &viewer.user_id, topic, ts + 3_000).await?;
        }
        if let Some(avatar) = &room.avatar {
            set_room_avatar(
                client,
                hs,
                &room_id,
                &viewer.user_id,
                &uploaded_mxc(&media, avatar)?,
                ts + 4_000,
            )
            .await?;
        }
        rooms.insert(
            room.id.clone(),
            DemoRoom {
                room_id,
                name: room.name.clone(),
                direct: room.direct,
                members: room.members.clone(),
            },
        );
    }

    if !direct.is_empty() {
        let map: serde_json::Value = direct
            .into_iter()
            .map(|(user, rooms)| (user, serde_json::json!(rooms)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        set_direct_account_data(client, hs, &viewer_token, &viewer.user_id, &map).await?;
    }

    // Spaces, once their children exist to be linked to. A space has no
    // timeline anyone reads, so its own state is left at the current time; the
    // `m.space.parent` written into each *child* is backdated, since that one
    // does land in a room a demo looks at.
    let mut spaces: BTreeMap<String, DemoSpace> = BTreeMap::new();
    for space in &corpus.space {
        let space_id = create_space(client, hs, &viewer_token).await?;
        set_room_name(client, hs, &space_id, &viewer.user_id, &space.name, now_ms).await?;
        if let Some(topic) = &space.topic {
            set_room_topic(client, hs, &space_id, &viewer.user_id, topic, now_ms).await?;
        }
        if let Some(avatar) = &space.avatar {
            set_room_avatar(
                client,
                hs,
                &space_id,
                &viewer.user_id,
                &uploaded_mxc(&media, avatar)?,
                now_ms,
            )
            .await?;
        }
        for child in &space.children {
            let child_id = &rooms
                .get(child)
                .ok_or_else(|| anyhow!("space {} names unknown room {child}", space.id))?
                .room_id;
            let ts = *setup_ts
                .get(child.as_str())
                .unwrap_or(&(now_ms - SETUP_LEAD_MS));
            add_space_child(client, hs, &viewer.user_id, &space_id, child_id, ts + 5_000).await?;
        }
        spaces.insert(
            space.id.clone(),
            DemoSpace {
                room_id: space_id,
                name: space.name.clone(),
                children: space.children.clone(),
            },
        );
    }

    let mut events: BTreeMap<String, String> = BTreeMap::new();
    let mut txn = 0usize;
    let mut next_txn = || {
        txn += 1;
        format!("corpus-{txn:04}")
    };
    for (ts, message) in ordered {
        let room_id = rooms
            .get(&message.room)
            .ok_or_else(|| anyhow!("message references unknown room {}", message.room))?
            .room_id
            .clone();
        let sender = user_id(&message.from)?;
        let content = message_content(message, &events, &media)?;
        let event_id = send_appservice_event(
            client,
            hs,
            &room_id,
            &sender,
            &next_txn(),
            "m.room.message",
            content,
            ts,
        )
        .await?;
        if let Some(id) = &message.id {
            events.insert(id.clone(), event_id.clone());
        }

        for (index, reaction) in message.reactions.iter().enumerate() {
            send_appservice_event(
                client,
                hs,
                &room_id,
                &user_id(&reaction.from)?,
                &next_txn(),
                "m.reaction",
                serde_json::json!({
                    "m.relates_to": {
                        "rel_type": "m.annotation",
                        "event_id": event_id,
                        "key": reaction.key
                    }
                }),
                ts + REACTION_OFFSET_MS * (index as i64 + 1),
            )
            .await?;
        }

        if let Some(edited) = &message.edited_to {
            send_appservice_event(
                client,
                hs,
                &room_id,
                &sender,
                &next_txn(),
                "m.room.message",
                serde_json::json!({
                    "msgtype": "m.text",
                    "body": format!("* {edited}"),
                    "m.new_content": { "msgtype": "m.text", "body": edited },
                    "m.relates_to": { "rel_type": "m.replace", "event_id": event_id }
                }),
                ts + EDIT_OFFSET_MS,
            )
            .await?;
        }

        if message.redacted {
            redact_appservice_event(
                client,
                hs,
                &room_id,
                &sender,
                &event_id,
                &next_txn(),
                ts + REDACTION_OFFSET_MS,
            )
            .await?;
        }
    }

    let personas_out = corpus
        .persona
        .iter()
        .map(|p| {
            Ok((
                p.id.clone(),
                DemoPersona {
                    user_id: user_id(&p.id)?,
                    display_name: p.display_name.clone(),
                },
            ))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;

    // Any joined room proves axon's sync has caught up with the seeding, so this
    // is deliberately *a* room and not *the* room — nothing downstream should
    // read significance into which one. `rooms` is a BTreeMap, so this is the
    // alphabetically first corpus room id: arbitrary, but stable across runs,
    // which is what a probe wants.
    let sync_probe_room = rooms
        .values()
        .map(|r| r.room_id.clone())
        .next()
        .ok_or_else(|| anyhow!("corpus declares no rooms"))?;

    Ok(SeededCorpus {
        viewer: viewer.clone(),
        sync_probe_room,
        manifest: DemoManifest {
            corpus_path: path,
            name: corpus.meta.name,
            description: corpus.meta.description,
            seeded_at: now,
            viewer: DemoViewer {
                persona: viewer_persona.id.clone(),
                account: viewer,
            },
            personas: personas_out,
            spaces,
            rooms,
            events,
            media: media
                .into_iter()
                .map(|(path, uploaded)| (path, uploaded.mxc))
                .collect(),
        },
    })
}

/// Catch the corpus mistakes that would otherwise surface as a confusing HTTP
/// error halfway through seeding, or — worse — as a demo that runs and is
/// quietly missing its point.
fn validate(corpus: &Corpus, now: chrono::DateTime<Utc>) -> anyhow::Result<()> {
    let mut seen = HashSet::new();
    for persona in &corpus.persona {
        if !seen.insert(persona.id.as_str()) {
            bail!("duplicate persona id {}", persona.id);
        }
    }
    let mut room_ids = HashSet::new();
    for room in &corpus.room {
        if !room_ids.insert(room.id.as_str()) {
            bail!("duplicate room id {}", room.id);
        }
        if room.direct && (room.name.is_some() || room.topic.is_some()) {
            bail!("corpus DM {} must have no name or topic", room.id);
        }
        for member in &room.members {
            if !seen.contains(member.as_str()) {
                bail!("room {} has unknown member {member}", room.id);
            }
        }
    }
    let mut space_ids = HashSet::new();
    for space in &corpus.space {
        if !space_ids.insert(space.id.as_str()) {
            bail!("duplicate space id {}", space.id);
        }
        for child in &space.children {
            if !room_ids.contains(child.as_str()) {
                bail!("space {} names unknown room {child}", space.id);
            }
        }
    }

    // Relations resolve against messages *already sent*, so a target has to be
    // strictly older than the message naming it — checked here rather than
    // discovered as an unresolvable handle mid-run.
    let mut defined: HashMap<&str, i64> = HashMap::new();
    let ordered = ordered_messages(corpus, now)?;
    for (ts, message) in &ordered {
        if !room_ids.contains(message.room.as_str()) {
            bail!(
                "message at {} names unknown room {}",
                message.at,
                message.room
            );
        }
        if !seen.contains(message.from.as_str()) {
            bail!(
                "message at {} has unknown sender {}",
                message.at,
                message.from
            );
        }
        if message.body.is_none() && message.image.is_none() {
            bail!(
                "message at {} in {} has neither body nor image",
                message.at,
                message.room
            );
        }
        if message.body.is_some() && message.image.is_some() {
            bail!(
                "message at {} in {} has both body and image; a caption goes in `caption`",
                message.at,
                message.room
            );
        }
        if message.html.is_some() != (message.format.as_deref() == Some("html")) {
            bail!(
                "message at {} in {} must set `format = \"html\"` and `html` together",
                message.at,
                message.room
            );
        }
        if message.edited_to.is_some() && message.redacted {
            // The redaction removes the content the edit produced, so the edit
            // is invisible in every client and the author silently did not get
            // what they wrote. `seed` applies the two independently and would
            // not notice.
            bail!(
                "message at {} in {} sets both `edited_to` and `redacted`; the redaction erases the edit",
                message.at,
                message.room
            );
        }
        for target in [&message.reply_to, &message.thread].into_iter().flatten() {
            match defined.get(target.as_str()) {
                Some(target_ts) if target_ts < ts => {}
                Some(_) => bail!(
                    "message at {} references {target}, which is not older",
                    message.at
                ),
                None => bail!("message at {} references undefined id {target}", message.at),
            }
        }
        for reaction in &message.reactions {
            if !seen.contains(reaction.from.as_str()) {
                bail!(
                    "reaction on {} has unknown sender {}",
                    message.at,
                    reaction.from
                );
            }
        }
        if let Some(id) = &message.id {
            if defined.insert(id.as_str(), *ts).is_some() {
                bail!("duplicate message id {id}");
            }
        }
    }
    Ok(())
}

/// `-6d 09:12` / `-1d 16:20:14` → epoch millis, relative to this run.
///
/// Seconds are optional but significant when present: the gallery run's spacing
/// is the whole point of that fixture.
/// Corpus messages paired with their resolved timestamps, oldest first.
///
/// Both `validate` and `seed` need exactly this, and both need it in the same
/// order: relations resolve against messages already sent, so "older" has to
/// mean the same thing in the check and in the run. Sharing `now` is part of
/// that — resolving the same relative offset against two different instants
/// could in principle have `parse_at`'s future check disagree between them.
fn ordered_messages(
    corpus: &Corpus,
    now: chrono::DateTime<Utc>,
) -> anyhow::Result<Vec<(i64, &Message)>> {
    let mut ordered: Vec<(i64, &Message)> = corpus
        .message
        .iter()
        .map(|m| Ok((parse_at(&m.at, now)?, m)))
        .collect::<anyhow::Result<_>>()?;
    ordered.sort_by_key(|(ts, _)| *ts);
    Ok(ordered)
}

fn parse_at(at: &str, now: chrono::DateTime<Utc>) -> anyhow::Result<i64> {
    let mut parts = at.split_whitespace();
    let (offset, time) = match (parts.next(), parts.next(), parts.next()) {
        (Some(offset), Some(time), None) => (offset, time),
        _ => bail!("timestamp {at:?} is not \"-<N>d HH:MM[:SS]\""),
    };
    let days: i64 = offset
        .strip_prefix('-')
        .and_then(|rest| rest.strip_suffix('d'))
        .ok_or_else(|| anyhow!("timestamp {at:?} offset must look like \"-6d\""))?
        .parse()
        .with_context(|| format!("parse day offset in {at:?}"))?;
    let time = match time.matches(':').count() {
        1 => NaiveTime::parse_from_str(time, "%H:%M"),
        2 => NaiveTime::parse_from_str(time, "%H:%M:%S"),
        _ => bail!("timestamp {at:?} time must be HH:MM or HH:MM:SS"),
    }
    .with_context(|| format!("parse time in {at:?}"))?;
    let resolved = (now - Duration::days(days))
        .date_naive()
        .and_time(time)
        .and_utc();
    if resolved > now {
        bail!("timestamp {at:?} resolves to the future; offsets must be at least a day");
    }
    Ok(resolved.timestamp_millis())
}

fn message_content(
    message: &Message,
    events: &BTreeMap<String, String>,
    media: &HashMap<String, Media>,
) -> anyhow::Result<serde_json::Value> {
    let mut content = match (&message.body, &message.image) {
        (Some(body), None) => {
            let mut content = serde_json::json!({ "msgtype": "m.text", "body": body });
            if let Some(html) = &message.html {
                content["format"] = serde_json::json!("org.matrix.custom.html");
                content["formatted_body"] = serde_json::json!(html);
            }
            content
        }
        (None, Some(image)) => {
            let uploaded = media
                .get(image)
                .ok_or_else(|| anyhow!("image {image} was never uploaded"))?;
            let filename = Path::new(image)
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow!("image path {image} has no file name"))?;
            let mut info = serde_json::json!({
                "mimetype": uploaded.mimetype,
                "size": uploaded.size,
            });
            if let Some((w, h)) = uploaded.dimensions {
                info["w"] = serde_json::json!(w);
                info["h"] = serde_json::json!(h);
            }
            // `filename` plus a differing `body` is what makes the body read as
            // a caption rather than as the file's name — both clients parse it
            // that way (`clients/web/src/media/parse-media.ts`).
            serde_json::json!({
                "msgtype": "m.image",
                "body": message.caption.as_deref().unwrap_or(filename),
                "filename": filename,
                "url": uploaded.mxc,
                "info": info,
            })
        }
        _ => bail!("message at {} is neither text nor image", message.at),
    };

    let resolve = |handle: &str| -> anyhow::Result<String> {
        events
            .get(handle)
            .cloned()
            .ok_or_else(|| anyhow!("message at {} references unsent id {handle}", message.at))
    };
    if let Some(thread) = &message.thread {
        let root = resolve(thread)?;
        content["m.relates_to"] = serde_json::json!({
            "rel_type": "m.thread",
            "event_id": root,
            // The reply fallback a thread member carries so clients without
            // thread support still render it as a reply to the root.
            "m.in_reply_to": { "event_id": root },
            "is_falling_back": true
        });
    } else if let Some(reply_to) = &message.reply_to {
        content["m.relates_to"] =
            serde_json::json!({ "m.in_reply_to": { "event_id": resolve(reply_to)? } });
    }
    Ok(content)
}

/// The `mxc://` for a corpus media path, or an error naming the path.
///
/// Every referenced path is uploaded up front, so a miss is unreachable today —
/// but indexing the map directly would panic rather than say which path is
/// missing, and the rest of this module reports that kind of gap as an error
/// (see `message_content`'s image lookup).
fn uploaded_mxc(media: &HashMap<String, Media>, rel: &str) -> anyhow::Result<String> {
    media
        .get(rel)
        .map(|uploaded| uploaded.mxc.clone())
        .ok_or_else(|| anyhow!("avatar {rel} was never uploaded"))
}

async fn upload_corpus_media(
    client: &Client,
    hs: &str,
    token: &str,
    media_root: &Path,
    rel: &str,
) -> anyhow::Result<Media> {
    let path = media_root.join(rel);
    let bytes = std::fs::read(&path).with_context(|| {
        format!(
            "read corpus media {} — generate-placeholder-photos.mjs writes stand-ins for the \
             photos that are not committed yet (corpus/media/README.md)",
            path.display()
        )
    })?;
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("corpus media path {rel} has no file name"))?
        .to_owned();
    let mimetype = mime_for(&path)?;
    let dimensions = image_dimensions(&bytes);
    let size = bytes.len();
    let mxc = upload_media(client, hs, token, &filename, &mimetype, bytes).await?;
    Ok(Media {
        mxc,
        mimetype,
        size,
        dimensions,
    })
}

fn mime_for(path: &Path) -> anyhow::Result<String> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Ok("image/png".to_owned()),
        Some("jpg" | "jpeg") => Ok("image/jpeg".to_owned()),
        Some("gif") => Ok("image/gif".to_owned()),
        Some("webp") => Ok("image/webp".to_owned()),
        _ => bail!("unsupported corpus media type: {}", path.display()),
    }
}

/// Pixel dimensions from the file header, for PNG and JPEG.
///
/// Worth the header walk rather than the `image` crate: `info.w`/`info.h` are
/// what the web client reserves aspect ratio with, and a gallery grid laid out
/// without them is a demo of the wrong thing.
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
    if bytes.starts_with(PNG_MAGIC) && bytes.len() >= 24 && &bytes[12..16] == b"IHDR" {
        let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((w, h));
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 3 < bytes.len() {
            if bytes[i] != 0xFF {
                return None;
            }
            let marker = bytes[i + 1];
            // Padding and standalone markers carry no length field.
            if marker == 0xFF {
                i += 1;
                continue;
            }
            if matches!(marker, 0xD8 | 0xD9) || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            let length = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            // Any start-of-frame; the four excluded values in this range are
            // Huffman/arithmetic tables and restart definitions.
            let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
            if is_sof {
                if i + 9 > bytes.len() {
                    return None;
                }
                let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
                return Some((w, h));
            }
            // Start of scan: entropy-coded data follows, so stop looking.
            if marker == 0xDA {
                return None;
            }
            i += 2 + length;
        }
    }
    None
}

/// A corpus room: private and invite-only. Name, topic, avatar and membership
/// all arrive afterwards as backdated state; a DM goes through `create_dm`.
async fn create_named_room(client: &Client, hs: &str, token: &str) -> anyhow::Result<String> {
    let resp: RoomResponse = client
        .post(format!("{hs}/_matrix/client/v3/createRoom"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "preset": "private_chat", "visibility": "private" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.room_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Corpus {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/demo.toml");
        let text = std::fs::read_to_string(&path).expect("read committed corpus");
        toml::from_str(&text).expect("committed corpus parses")
    }

    #[test]
    fn committed_corpus_is_valid() {
        validate(&corpus(), Utc::now()).expect("committed corpus validates");
    }

    #[test]
    fn an_edited_message_may_not_also_be_redacted() {
        // Seeding both is always a mistake: the redaction erases the content the
        // edit produced, so the edit renders nowhere and the corpus author gets
        // something other than what they wrote.
        let mut corpus = corpus();
        let message = corpus
            .message
            .iter_mut()
            .find(|m| m.edited_to.is_some())
            .expect("committed corpus has an edited message");
        message.redacted = true;

        let err = validate(&corpus, Utc::now()).expect_err("must be rejected");
        assert!(
            err.to_string().contains("erases the edit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn committed_corpus_stays_under_the_timeline_limit() {
        // Every room has to fit in one sync window until historical backfill
        // lands (issue 164) — a room that overflows silently loses its oldest
        // messages, which is exactly what a demo would be showing.
        let corpus = corpus();
        let mut per_room: HashMap<&str, usize> = HashMap::new();
        for message in &corpus.message {
            // A message can be up to three events: itself, an edit, a
            // redaction — plus one per reaction.
            let extra = usize::from(message.edited_to.is_some())
                + usize::from(message.redacted)
                + message.reactions.len();
            *per_room.entry(message.room.as_str()).or_default() += 1 + extra;
        }
        for (room, count) in per_room {
            assert!(count < 200, "room {room} seeds {count} events");
        }
    }

    #[test]
    fn relative_timestamps_resolve_into_the_past() {
        let now = "2026-08-01T09:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .expect("fixed now");
        let day = parse_at("-1d 16:20", now).expect("minute precision");
        let second = parse_at("-1d 16:20:14", now).expect("second precision");
        assert_eq!(second - day, 14_000, "seconds must survive parsing");
        assert!(day < now.timestamp_millis());
        // The gallery run groups on a 60s window; the near-miss must not.
        let near_miss = parse_at("-1d 16:39", now).expect("near miss");
        assert!(near_miss - parse_at("-1d 16:20:49", now).expect("run tail") > 60_000);
    }

    #[test]
    fn malformed_timestamps_are_rejected() {
        let now = Utc::now();
        for bad in [
            "6d 09:12",
            "-6 09:12",
            "-6d",
            "-6d 09:12:00:00",
            "-6d 25:00",
        ] {
            assert!(parse_at(bad, now).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn jpeg_and_png_headers_yield_dimensions() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&256u32.to_be_bytes());
        png.extend_from_slice(&128u32.to_be_bytes());
        assert_eq!(image_dimensions(&png), Some((256, 128)));

        // SOI, a JFIF APP0 segment to be skipped, then SOF0.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        jpeg.extend_from_slice(&1067u16.to_be_bytes());
        jpeg.extend_from_slice(&1600u16.to_be_bytes());
        jpeg.extend_from_slice(&[0x03; 10]);
        assert_eq!(image_dimensions(&jpeg), Some((1600, 1067)));

        assert_eq!(image_dimensions(b"not an image"), None);
    }
}
