//! The scripted scenes.
//!
//! A scene is a list of steps against the demo corpus (ADR 0086). Two rules
//! shape all of them:
//!
//! - **Address content by corpus name, never by Matrix id.** Room ids and event
//!   ids are per-run; `manifest.demo` maps the stable corpus names onto them.
//! - **Wait on a screen predicate, then dwell.** The wait is the correctness
//!   gate — it is what keeps a recording from desyncing on a slow machine — and
//!   the dwell is purely so a viewer can read the frame. Never sleep instead of
//!   waiting.
//!
//! Scenes are coupled to the corpus prose: the needles below are substrings of
//! message bodies in `smoke/local-stack/corpus/demo.toml`. Rewording a message
//! there fails the scene loudly, with the rendered screen attached, which is the
//! intended failure mode — a demo that silently stops demonstrating anything is
//! the one thing `docs/demo-coverage.md` exists to prevent.

use std::time::Duration;

use anyhow::{anyhow, bail};
use axon_smoke_tui::local_stack::Demo;

/// One scripted action.
pub enum Step {
    /// Type into the input line and submit.
    Command(String),
    /// Type literal text without submitting.
    Type(String),
    /// Send raw key bytes, with a label for the run log.
    Key(&'static str, Vec<u8>),
    /// Block until the rendered screen contains every needle.
    WaitFor(Vec<String>),
    /// Block until the text is on screen *outside* the input area.
    ///
    /// A sent message is on screen the moment it is typed — in the input line —
    /// so a plain `WaitFor` on the body would be satisfied before Enter was
    /// even pressed. This waits for the text to appear somewhere that is not
    /// the bottom of the screen, which is the round trip actually completing.
    WaitSent(String),
    /// Block until the rendered screen contains none of the needles.
    ///
    /// The counterpart that makes a narrowing step provable. `WaitFor` alone
    /// cannot tell "the filter applied" from "the thing I am looking for was
    /// already on screen", and a step that is satisfied by the *previous* frame
    /// lets the script run ahead of the client — which is how a scene desyncs
    /// and a recording captures a keystroke landing in the wrong place.
    WaitGone(Vec<String>),
    /// Hold the frame so a viewer can read it. Scaled by `--pace`.
    Dwell(Duration),
}

pub struct Scene {
    pub name: String,
    pub steps: Vec<Step>,
}

/// Long enough to read a line; long enough to take in a whole screen.
const BEAT: Duration = Duration::from_millis(1200);
const LONG: Duration = Duration::from_millis(2600);

/// Every scene name, in the order `tour` plays them.
pub const SCENE_NAMES: &[&str] = &[
    "rooms",
    "timeline",
    "react",
    "threads",
    "media",
    "search",
    "jump",
    "send",
    "shortcuts",
];

impl Scene {
    pub fn build(name: &str, demo: &Demo) -> anyhow::Result<Self> {
        let steps = match name {
            "tour" => {
                let mut steps = Vec::new();
                for scene in SCENE_NAMES {
                    steps.extend(build_one(scene, demo)?);
                }
                steps
            }
            other => build_one(other, demo)?,
        };
        Ok(Self {
            name: name.to_owned(),
            steps,
        })
    }
}

fn build_one(name: &str, demo: &Demo) -> anyhow::Result<Vec<Step>> {
    match name {
        "rooms" => rooms(demo),
        "timeline" => timeline(demo),
        "react" => react(demo),
        "threads" => threads(demo),
        "media" => media(demo),
        "search" => search(demo),
        "jump" => jump(demo),
        "send" => send(demo),
        "shortcuts" => Ok(shortcuts()),
        other => bail!(
            "unknown scene {other:?}; expected `tour` or one of: {}",
            SCENE_NAMES.join(", ")
        ),
    }
}

/// The room list: sort, filter, and the live name filter (ADR 0042).
///
/// Each narrowing step is proved by what *left* the list, not by what stayed:
/// `gear-swap` appears nowhere but the room list, which makes it a clean
/// witness for a filter having actually applied.
fn rooms(demo: &Demo) -> anyhow::Result<Vec<Step>> {
    let general = room_name(demo, "general")?;
    let trail = room_name(demo, "trail-work")?;
    let photos = room_name(demo, "trip-photos")?;
    let gear = room_name(demo, "gear-swap")?;
    let maya = persona_name(demo, "maya")?;

    Ok(vec![
        wait(&[&general, &trail, &photos, &gear]),
        Step::Dwell(LONG),
        cmd("/filter groups"),
        wait(&["filter: Groups", &trail]),
        Step::Dwell(BEAT),
        cmd("/filter dms"),
        // The DM has no name of its own; both clients derive its title from the
        // other member, which is exactly what this frame shows.
        wait(&["filter: DMs", &maya]),
        gone(&[&gear]),
        Step::Dwell(LONG),
        cmd("/filter all"),
        wait(&["filter: All", &gear]),
        Step::Dwell(BEAT),
        cmd("/sort az"),
        wait(&["sort: A"]),
        Step::Dwell(LONG),
        cmd("/sort recent"),
        wait(&["sort: Recent"]),
        Step::Dwell(BEAT),
        // Alt-/ opens the live name filter and narrows as it is typed.
        alt('/'),
        wait(&["Filter"]),
        Step::Type("trail".to_owned()),
        wait(&[&trail]),
        gone(&[&gear]),
        Step::Dwell(LONG),
        escape(),
        wait(&[&gear]),
        Step::Dwell(BEAT),
    ])
}

/// A room's timeline: formatted text, replies, and reaction badges.
fn timeline(demo: &Demo) -> anyhow::Result<Vec<Step>> {
    let general = room_name(demo, "general")?;
    Ok(vec![
        cmd(&format!("/room {general}")),
        wait(&[&general, "driving up from the south side"]),
        Step::Dwell(LONG),
        // The HTML message: bold lead-in and a link, rendered rather than shown
        // as markup. `Twenty optimistic minutes` carries the reaction badges.
        wait(&["Parking note", "Twenty optimistic minutes"]),
        Step::Dwell(LONG),
    ])
}

/// Add a reaction, then withdraw it.
///
/// Both halves are scripted on purpose. Withdrawing is a real affordance worth
/// showing, and it also leaves the room exactly as it was found — which is what
/// keeps this scene repeatable *and* keeps its assertions honest. A react-only
/// scene passes vacuously the second time it runs, because the badge it waits
/// for is already there from the first.
///
/// The target is Tomás's lift-share message, chosen because it is the one
/// message in `#general` carrying no seeded reactions, so the badge appearing is
/// unambiguous. `🎉` likewise appears nowhere else in this room.
fn react(demo: &Demo) -> anyhow::Result<Vec<Step>> {
    let general = room_name(demo, "general")?;
    Ok(vec![
        cmd(&format!("/room {general}")),
        wait(&[&general, "driving up from the south side"]),
        Step::Dwell(BEAT),
        // Select the target by text rather than by position: it is mid-room, and
        // counting Ctrl-J presses would break the moment the corpus grows.
        ctrl_j(),
        ctrl_f(),
        wait(&["Search:"]),
        Step::Type("driving up from the south side".to_owned()),
        enter(),
        Step::Dwell(BEAT),
        // Shift-R opens the reaction prompt; its status line is the proof, since
        // the message it targets was already on screen.
        Step::Key("Shift-R", b"R".to_vec()),
        wait(&["type emoji"]),
        Step::Dwell(BEAT),
        // Shortcodes resolve through the emoji table, so the demo shows the name
        // being typed rather than a pasted glyph.
        Step::Type("tada".to_owned()),
        Step::Dwell(BEAT),
        enter(),
        wait(&["🎉"]),
        Step::Dwell(LONG),
        // `/unreact`, not the Shift-U chord: sending the reaction returns the
        // input to compose mode, where a bare `U` is just a character typed into
        // the buffer. The command acts on the still-selected message. With
        // exactly one of our own reactions on it, nothing needs disambiguating.
        cmd("/unreact"),
        gone(&["🎉"]),
        Step::Dwell(BEAT),
    ])
}

/// Threads, edits, and redactions.
fn threads(demo: &Demo) -> anyhow::Result<Vec<Step>> {
    let trail = room_name(demo, "trail-work")?;
    Ok(vec![
        cmd(&format!("/room {trail}")),
        wait(&[&trail, "Switchback 14 is done"]),
        Step::Dwell(BEAT),
        // The edit is applied in place: the timeline shows the new body, not
        // the original "6 rock bars".
        wait(&["Bringing 4 rock bars"]),
        // The redacted message keeps its place as a tombstone.
        wait(&["[redacted]"]),
        Step::Dwell(LONG),
        // Reach the thread root the way a person would: move into the message
        // pane, find it by text, open the thread on the selection.
        //
        // Not `/unreadthreads`: the picker is empty here. The corpus viewer is
        // a member of every room from before its first message, so nothing is
        // unread for them — the picker needs a room the viewer has fallen
        // behind in, which the corpus does not currently build.
        ctrl_j(),
        ctrl_f(),
        wait(&["Search:"]),
        Step::Type("Planning thread".to_owned()),
        Step::Dwell(BEAT),
        enter(),
        wait(&["Planning thread"]),
        Step::Dwell(BEAT),
        // `t` opens the thread on the selected message. Both waits are on
        // state-unique chrome — the `[in thread]` title marker — plus a reply
        // that lives *only* inside the thread, since the root itself is
        // visible either way.
        Step::Key("t", b"t".to_vec()),
        wait(&["[in thread]", "rather rebuild three switchbacks"]),
        Step::Dwell(LONG),
        escape(),
        gone(&["[in thread]"]),
        Step::Dwell(BEAT),
    ])
}

/// Inline graphics. The reason this pilot exists at all: a headless recorder
/// renders none of this.
fn media(demo: &Demo) -> anyhow::Result<Vec<Step>> {
    let photos = room_name(demo, "trip-photos")?;
    Ok(vec![
        cmd(&format!("/room {photos}")),
        wait(&[&photos, "the light on the talus"]),
        // Decoding and encoding the images takes a beat longer than the text
        // around them; the captions are on screen before the pixels are.
        Step::Dwell(LONG),
        wait(&["Cribbing going in"]),
        Step::Dwell(LONG),
        // Open an image full size. Ctrl-J moves the selection into the message
        // pane, End drops to the bottom, and Ctrl-K steps back one — over the
        // viewer's own join, which is the newest event in every corpus room
        // because `/createRoom` does not honour a backdated `ts`.
        //
        // The preview titles itself with the filename, so waiting on that
        // rather than on the bare "Esc to close" is what proves the selection
        // landed on the image and not on the row above or below it.
        ctrl_j(),
        end(),
        ctrl_k(),
        Step::Dwell(BEAT),
        Step::Key("v", b"v".to_vec()),
        wait(&["talus-light.jpg", "Esc to close"]),
        Step::Dwell(LONG),
        escape(),
        gone(&["Esc to close"]),
        Step::Dwell(BEAT),
    ])
}

/// Full-text search across rooms, narrowed by field filters. The corpus puts
/// "switchback" in three rooms and five senders so each filter visibly narrows
/// the result set instead of returning the same list.
fn search(demo: &Demo) -> anyhow::Result<Vec<Step>> {
    let photos = room_name(demo, "trip-photos")?;
    let trail = room_name(demo, "trail-work")?;
    Ok(vec![
        // "Search Results" is the pane's own title and "result 1 of N" its
        // status: together they prove a search ran, where the query text alone
        // would not — that is sitting in the input line either way.
        //
        // The room label on the hits is the witness for breadth, rather than a
        // particular message: the result view's starting sort order is not
        // fixed, so which individual hits are above the fold is not either.
        cmd("/search room:* switchback"),
        wait(&["Search Results", "result 1 of", &trail, &photos]),
        Step::Dwell(LONG),
        // Esc back to compose *before* the next command. The result view binds
        // bare letters to its own actions (s sort, g group, e edit, r reply,
        // t thread), so a command typed while it has focus is not typed at all
        // — it is read as that sequence of shortcuts.
        escape(),
        gone(&["Search Results"]),
        // Narrowing to one room must remove every trail-work hit. The results
        // pane covers the room list, so the room name cannot leak in from
        // there; its absence really is the filter working.
        cmd(&format!("/search room:{photos} switchback")),
        wait(&["Search Results", "result 1 of", &photos]),
        gone(&[&trail]),
        Step::Dwell(LONG),
        // The result view's own affordances (`s` sort, `g` group, `e` edit) are
        // deliberately not scripted. Each reloads asynchronously and restores
        // the result view when it lands, so an Esc that follows one can be
        // undone by it — and their labels cannot be waited on, because the
        // starting order is not fixed. Left uncovered in
        // `docs/demo-coverage.md` rather than scripted as a race.
        escape(),
        gone(&["Search Results"]),
        Step::Dwell(BEAT),
    ])
}

/// Jump to a date in history. The corpus is backdated across five weeks, so
/// there is real distance to travel.
fn jump(demo: &Demo) -> anyhow::Result<Vec<Step>> {
    let trail = room_name(demo, "trail-work")?;
    // Counted back from the manifest's `seeded_at`, not from this process's
    // `now`: fourteen days before the *seeding* run is the day the planning
    // thread opened, and a stack brought up by `demo-stack.sh up` is meant to
    // stay up for a recording that may happen on a later calendar day. Using
    // `now` here made the scene fail whenever `up` and `record` straddled
    // midnight — it jumped to a date the corpus never wrote to.
    let target = (demo.seeded_at - chrono::Duration::days(14))
        .format("%Y-%m-%d")
        .to_string();
    Ok(vec![
        cmd(&format!("/room {trail}")),
        wait(&[&trail]),
        Step::Dwell(BEAT),
        cmd(&format!("/jump {target}")),
        // The status line the jump writes, not a message body: every corpus
        // room is small enough to fit on one screen, so *any* body needle is
        // already satisfied before the jump and would prove nothing.
        wait(&[&format!("Jumped to {target}"), "Planning thread"]),
        Step::Dwell(LONG),
    ])
}

/// Send a message and watch it land — the only scene that writes to the
/// homeserver. Safe because the demo stack is disposable and local; the
/// live-account rule in `clients/*/AGENTS.md` is about remote homeservers.
fn send(demo: &Demo) -> anyhow::Result<Vec<Step>> {
    let maya = persona_name(demo, "maya")?;
    // By id, not by title: `/room` resolves an id, a canonical alias, or a
    // room *name*, and a DM has no name — the "Maya Harrison" in the room list
    // is derived per-render from the other member, and `/room Maya Harrison`
    // answers "room not found". Recorded in `docs/demo-coverage.md`.
    let dm = room_id(demo, "dm-maya")?;
    Ok(vec![
        cmd(&format!("/room {dm}")),
        // On the status line the switch writes, not on a seeded message: this
        // is the one scene that *adds* to its room, so on a long-lived stack it
        // pushes the room's oldest messages off screen — including any needle
        // taken from them. The status names the room we asked for and is true
        // however much history has accumulated.
        wait(&[&format!("showing {dm}"), &maya]),
        Step::Dwell(BEAT),
        // Must not begin with a capital J: on an empty input that is the
        // date-jump shortcut, by design (`keymap.rs`), so the rest of the line
        // would be typed into a date prompt.
        Step::Type("Pushed the turbidity numbers into section 3.".to_owned()),
        Step::Dwell(BEAT),
        enter(),
        Step::WaitSent("turbidity numbers into section 3".to_owned()),
        Step::Dwell(LONG),
    ])
}

/// The two reference popups. Last, so the tour ends on what a new user would
/// press next.
fn shortcuts() -> Vec<Step> {
    vec![
        cmd("/shortcuts"),
        wait(&["Shortcuts", "cycle focus"]),
        Step::Dwell(LONG),
        // Each Esc is followed by a wait for the popup to actually be gone.
        // Without it the next command is typed into a popup that is still up —
        // which swallows it whole — and the scene only survives on the dwell
        // being long enough, so it breaks at `--pace 0`.
        escape(),
        gone(&["cycle focus"]),
        Step::Dwell(BEAT),
        cmd("/help"),
        wait(&["Help", "Enter to select"]),
        Step::Dwell(LONG),
        escape(),
        gone(&["Enter to select"]),
        Step::Dwell(BEAT),
    ]
}

// ── step constructors ───────────────────────────────────────────────────────

fn cmd(command: &str) -> Step {
    Step::Command(command.to_owned())
}

fn wait(needles: &[&str]) -> Step {
    Step::WaitFor(needles.iter().map(|n| (*n).to_owned()).collect())
}

fn gone(needles: &[&str]) -> Step {
    Step::WaitGone(needles.iter().map(|n| (*n).to_owned()).collect())
}

fn enter() -> Step {
    Step::Key("Enter", b"\r".to_vec())
}

fn escape() -> Step {
    Step::Key("Esc", vec![0x1b])
}

/// Terminals encode Alt-<char> as ESC followed by the character in one write,
/// which crossterm parses as `KeyModifiers::ALT`.
fn alt(ch: char) -> Step {
    Step::Key("Alt-/", vec![0x1b, ch as u8])
}

/// `message_down`: moves the selection into the message pane.
fn ctrl_j() -> Step {
    Step::Key("Ctrl-J", vec![0x0a])
}

/// `message_up`: steps the selection one message toward the older end.
fn ctrl_k() -> Step {
    Step::Key("Ctrl-K", vec![0x0b])
}

/// `find`: starts an in-timeline text search over the open room.
fn ctrl_f() -> Step {
    Step::Key("Ctrl-F", vec![0x06])
}

/// `End`: with the selection in the message pane, drops to the newest message.
fn end() -> Step {
    Step::Key("End", b"\x1b[F".to_vec())
}

// ── corpus lookups ──────────────────────────────────────────────────────────

fn room_name(demo: &Demo, id: &str) -> anyhow::Result<String> {
    let room = demo
        .rooms
        .get(id)
        .ok_or_else(|| missing("room", id, demo.rooms.keys()))?;
    room.name
        .clone()
        .ok_or_else(|| anyhow!("corpus room {id:?} has no name; address it by member instead"))
}

/// The per-run Matrix id. Needed only where a room cannot be addressed by
/// name, which today means the DM.
fn room_id(demo: &Demo, id: &str) -> anyhow::Result<String> {
    Ok(demo
        .rooms
        .get(id)
        .ok_or_else(|| missing("room", id, demo.rooms.keys()))?
        .room_id
        .clone())
}

fn persona_name(demo: &Demo, id: &str) -> anyhow::Result<String> {
    Ok(demo
        .personas
        .get(id)
        .ok_or_else(|| missing("persona", id, demo.personas.keys()))?
        .display_name
        .clone())
}

fn missing<'a>(kind: &str, id: &str, known: impl Iterator<Item = &'a String>) -> anyhow::Error {
    let known: Vec<&str> = known.map(String::as_str).collect();
    anyhow!(
        "corpus has no {kind} {id:?}; the manifest lists: {}",
        known.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest with one room, seeded at a fixed instant well in the past.
    fn demo_seeded_at(seeded_at: &str) -> Demo {
        serde_json::from_value(serde_json::json!({
            "name": "test",
            "seeded_at": seeded_at,
            "viewer": {
                "persona": "alex",
                "account": { "user_id": "@alex:localhost", "password": "pw" },
            },
            "personas": {},
            "spaces": {},
            "rooms": {
                "trail-work": {
                    "room_id": "!trail:localhost",
                    "name": "Trail Work",
                    "direct": false,
                    "members": [],
                },
            },
            "events": {},
            "media": {},
        }))
        .expect("fixture matches the manifest shape")
    }

    /// The `jump` scene must date its target from the manifest, not from the
    /// clock: `demo-stack.sh up` is designed to leave a stack running for a
    /// recording made later, possibly on another calendar day. Computed from
    /// `Utc::now()` this asserted a date the corpus never wrote to, and the
    /// scene timed out waiting for a status line that never appeared.
    #[test]
    fn jump_targets_fourteen_days_before_seeding() {
        let demo = demo_seeded_at("2026-01-20T09:00:00Z");
        let steps = Scene::build("jump", &demo).expect("build jump scene");
        let commands: Vec<&str> = steps
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Command(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            commands.contains(&"/jump 2026-01-06"),
            "jump scene should target 2026-01-06, got {commands:?}"
        );
    }

    /// The same manifest read twice must produce the same date, whatever the
    /// clock says — the property the scene actually depends on.
    #[test]
    fn jump_target_does_not_move_with_the_clock() {
        let early = Scene::build("jump", &demo_seeded_at("2026-01-20T00:00:01Z"))
            .expect("build jump scene");
        let late = Scene::build("jump", &demo_seeded_at("2026-01-20T23:59:59Z"))
            .expect("build jump scene");
        let dates = |scene: &Scene| -> Vec<String> {
            scene
                .steps
                .iter()
                .filter_map(|step| match step {
                    Step::Command(text) => text.strip_prefix("/jump ").map(str::to_owned),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(dates(&early), vec!["2026-01-06".to_owned()]);
        assert_eq!(dates(&early), dates(&late));
    }
}
