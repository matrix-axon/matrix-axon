//! S1 TUI smoke scenarios.
//!
//! Each spawns the real `axon-tui` against a fresh in-process stub, asserts on
//! the rendered screen and the stub's request journal, and tears the process
//! down. Assertions read the parsed terminal screen; exit assertions also
//! require the alternate-screen leave sequence so a clean process exit cannot
//! hide a terminal-restoration regression.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};

use crate::runner::{Ctx, ScenarioOutcome};
use crate::stub::{JournalEntry, Stub, StubState};
use axon_smoke_tui::pty::PtyDriver;

/// `launch_and_quit`: first paint renders the room, message, and input panes,
/// then `/quit` exits cleanly with the terminal restored.
pub async fn launch_and_quit(ctx: &Ctx) -> ScenarioOutcome {
    let stub = match Stub::start(&ctx.run_id).await {
        Ok(stub) => stub,
        Err(err) => return failed_before_spawn(err),
    };
    let mut driver = match ctx.spawn_tui("launch_and_quit", &stub.base_url()) {
        Ok(driver) => driver,
        Err(err) => {
            stub.stop().await;
            return failed_before_spawn(err);
        }
    };

    let result = (|| {
        wait_first_paint(&mut driver, &stub.state, ctx.timeout)?;
        if !driver.saw_alt_screen_enter() {
            bail!("TUI never entered the alternate screen");
        }
        driver.type_text("/quit")?;
        driver.press_enter()?;
        require_clean_exit(&mut driver, ctx.timeout)
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    stub.stop().await;
    outcome
}

/// `ctrl_c_exit`: the configured Ctrl-C shortcut exits cleanly.
pub async fn ctrl_c_exit(ctx: &Ctx) -> ScenarioOutcome {
    let stub = match Stub::start(&ctx.run_id).await {
        Ok(stub) => stub,
        Err(err) => return failed_before_spawn(err),
    };
    let mut driver = match ctx.spawn_tui("ctrl_c_exit", &stub.base_url()) {
        Ok(driver) => driver,
        Err(err) => {
            stub.stop().await;
            return failed_before_spawn(err);
        }
    };

    let result = (|| {
        wait_first_paint(&mut driver, &stub.state, ctx.timeout)?;
        driver.press_ctrl_c()?;
        require_clean_exit(&mut driver, ctx.timeout)
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    stub.stop().await;
    outcome
}

/// `send_round_trip`: a run-marked message is submitted via keystrokes, the
/// stub's journal records the send, and the WebSocket echo renders in the room.
pub async fn send_round_trip(ctx: &Ctx) -> ScenarioOutcome {
    let stub = match Stub::start(&ctx.run_id).await {
        Ok(stub) => stub,
        Err(err) => return failed_before_spawn(err),
    };
    let mut driver = match ctx.spawn_tui("send_round_trip", &stub.base_url()) {
        Ok(driver) => driver,
        Err(err) => {
            stub.stop().await;
            return failed_before_spawn(err);
        }
    };

    // Use the first 8 chars of the UUID (32-bit isolation). The full run_id
    // would make the marker 46 chars which wraps across rows in the message
    // pane, breaking the screen.contains() check (vt100 splits at column edges).
    let marker = format!("roundtrip-{}", &ctx.run_id[..8]);
    let result = (|| {
        wait_first_paint(&mut driver, &stub.state, ctx.timeout)?;
        // Connect-before-trigger: `/v1/ws` is a live tail with no replay, so
        // wait for the TUI's WS upgrade before sending.
        wait_for_journal(&stub.state, ctx.timeout, "GET /v1/ws", |entries| {
            entries
                .iter()
                .any(|e| e.method == "GET" && e.path == "/v1/ws")
        })?;

        driver.type_text(&marker)?;
        driver.press_enter()?;

        // The journal must record the send with our exact body.
        wait_for_journal(&stub.state, ctx.timeout, "the send request", |entries| {
            entries.iter().any(|e| is_send_of(e, &marker))
        })?;

        // The WS echo of that send must render in the open room.
        driver.wait_for_screen("the echoed message to render", ctx.timeout, |screen| {
            screen.contains(&marker)
        })?;
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    driver.terminate();
    drop(driver);
    stub.stop().await;
    outcome
}

/// Wait until the stub profile has actually painted: room list, input line,
/// **and** the seeded timeline.
///
/// The timeline assertion is the important one. Without it, first paint was
/// satisfied by the chrome alone, so a page the TUI rejected wholesale looked
/// identical to a page it rendered — which is how #190 hid: the stub's
/// `EventDto` was missing `arrival_order`, every timeline response failed to
/// deserialize, and *every* stub scenario ran against an empty message pane
/// without noticing. Only the one scenario that needed a request made on the
/// success path went red, and it reported a missing `/threads` rather than the
/// empty timeline that caused it.
///
/// Every stub scenario is seeded with the "smoke seed" message by
/// `get_timeline`, so this holds for all of them.
fn wait_first_paint(
    driver: &mut PtyDriver,
    state: &StubState,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    wait_for_room_and_input_until(driver, &state.room_name, deadline)?;
    driver.wait_for_screen_or_exit(
        "the seeded timeline to render",
        deadline.saturating_duration_since(Instant::now()),
        |screen| screen.contains("smoke seed"),
    )?;
    Ok(())
}

fn wait_for_room_and_input(
    driver: &mut PtyDriver,
    room_name: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    wait_for_room_and_input_until(driver, room_name, Instant::now() + timeout)
}

fn wait_for_room_and_input_until(
    driver: &mut PtyDriver,
    room_name: &str,
    deadline: Instant,
) -> anyhow::Result<()> {
    let room_name = room_name.to_owned();
    driver.wait_for_screen_or_exit(
        "the room list to render",
        deadline.saturating_duration_since(Instant::now()),
        move |screen| screen.contains("Rooms") && screen.contains(&room_name),
    )?;
    // Anchor to the bottom section of the screen where the input box lives,
    // not the entire screen — the room-list selection marker "> " renders at
    // the top and would satisfy screen.contains('>') immediately.
    driver.wait_for_screen_or_exit(
        "the input line to render",
        deadline.saturating_duration_since(Instant::now()),
        |screen| screen.lines().rev().take(5).any(|l| l.contains('>')),
    )?;
    Ok(())
}

/// Require a zero-status exit within the deadline, with the alternate screen
/// left (terminal restored). A forced kill here would be a failure, not success.
fn require_clean_exit(driver: &mut PtyDriver, timeout: Duration) -> anyhow::Result<()> {
    let status = driver.wait_for_exit(timeout)?;
    if !status.success() {
        bail!("TUI exited with failure status: {status:?}");
    }
    // The leave sequence is emitted during teardown; give the reader thread up
    // to the full scenario timeout to drain the final bytes after exit.
    let deadline = Instant::now() + timeout;
    while !driver.saw_alt_screen_leave() {
        if Instant::now() >= deadline {
            bail!("TUI exited without leaving the alternate screen (terminal not restored)");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// Poll the stub journal until `predicate` holds.
fn wait_for_journal<F>(
    state: &StubState,
    timeout: Duration,
    what: &str,
    predicate: F,
) -> anyhow::Result<()>
where
    F: Fn(&[JournalEntry]) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        if predicate(&state.journal()) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            // Name what *did* arrive. Without it the message says only which
            // request never came, which cannot distinguish "the TUI asked and
            // the stub rejected it" from "the TUI never got far enough to ask"
            // — two failures with completely different causes.
            let seen = state.journal();
            let served = if seen.is_empty() {
                "nothing at all".to_owned()
            } else {
                seen.iter()
                    .map(|e| format!("{} {}", e.method, e.path))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return Err(anyhow!(
                "timed out after {timeout:?} waiting for {what}; stub served: {served}"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether `entry` is a message send carrying exactly `marker` as its body.
fn is_send_of(entry: &JournalEntry, marker: &str) -> bool {
    entry.method == "POST"
        && entry.path.ends_with("/send")
        && entry
            .body
            .as_ref()
            .and_then(|b| b.get("body"))
            .and_then(|b| b.as_str())
            == Some(marker)
}

/// `border_integrity`: seeds messages containing East-Asian Ambiguous chars
/// (`·` U+00B7, `■` U+25A0) and asserts that no text overflows into the
/// rightmost column of the terminal after first paint.
pub async fn border_integrity(ctx: &Ctx) -> ScenarioOutcome {
    let seed_body = "border · test ■ East Asian Ambiguous chars · should · not ■ overflow · the · right ■ border · column ■ in · a ■ terminal".to_owned();
    let stub = match Stub::start_with_seeds(&ctx.run_id, vec![seed_body]).await {
        Ok(stub) => stub,
        Err(err) => return failed_before_spawn(err),
    };
    let mut driver = match ctx.spawn_tui("border_integrity", &stub.base_url()) {
        Ok(driver) => driver,
        Err(err) => {
            stub.stop().await;
            return failed_before_spawn(err);
        }
    };

    let result = (|| {
        wait_first_paint(&mut driver, &stub.state, ctx.timeout)?;

        let rightmost = crate::runner::COLS - 1;
        let col = driver.col_chars(rightmost);

        const PERMITTED: &[&str] = &[
            "│", "─", "┌", "┐", "└", "┘", "║", "═", "╔", "╗", "╚", "╝", " ", "",
        ];
        let overflows: Vec<(u16, String)> = col
            .into_iter()
            .filter(|(_, ch)| !PERMITTED.contains(&ch.as_str()))
            .collect();

        if !overflows.is_empty() {
            let detail: String = overflows
                .iter()
                .map(|(row, ch)| format!("  row {row}: {:?}", ch))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "text overflowed into rightmost column (col {rightmost}):\n{detail}\n--- screen ---\n{}",
                driver.screen_text()
            );
        }

        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    stub.stop().await;
    outcome
}

/// `scroll_pin_on_relation_refresh`: verifies that when `apply_relation_outcome`
/// fires (triggered by the `GET .../threads` response), the viewport does not
/// advance after the initial tail position has been materialized.
///
/// Root cause of the regression this guards: after the first draw, the app
/// stores a concrete scroll offset instead of the initial `usize::MAX`
/// follow-tail sentinel. Adding a thread badge inflates `total_lines` and
/// therefore `max_scroll`; a later draw must clamp the stored offset, not snap
/// to the new bottom. Otherwise content near the old viewport boundary scrolls
/// off the top.
///
/// Assertion strategy: seed 30 extra messages so that the message area
/// overflows. The stub returns API pages newest-first (`seed`, then
/// `filler-00` ... `filler-29`), and the TUI reverses them before rendering, so
/// the 30x100 tail viewport contains `filler-11` through `filler-00` plus the
/// seed message. The thread badge on the seed message adds one line at the
/// bottom; without the pin, the viewport advances and `filler-11` scrolls off.
pub async fn scroll_pin_on_relation_refresh(ctx: &Ctx) -> ScenarioOutcome {
    let extras: Vec<String> = (0..30_u32).map(|i| format!("filler-{i:02}")).collect();
    let seed_id = format!("$seed-{}", ctx.run_id);
    let stub =
        match Stub::start_with_seeds_and_threads(&ctx.run_id, extras, vec![(seed_id, 3)]).await {
            Ok(stub) => stub,
            Err(err) => return failed_before_spawn(err),
        };
    let mut driver = match ctx.spawn_tui("scroll_pin_on_relation_refresh", &stub.base_url()) {
        Ok(driver) => driver,
        Err(err) => {
            stub.stop().await;
            return failed_before_spawn(err);
        }
    };

    let result = (|| {
        wait_first_paint(&mut driver, &stub.state, ctx.timeout)?;

        // The TUI calls spawn_relations_refresh after loading the timeline;
        // wait for that GET to land in the journal.
        wait_for_journal(&stub.state, ctx.timeout, "GET .../threads", |entries| {
            entries
                .iter()
                .any(|e| e.method == "GET" && e.path.ends_with("/threads"))
        })?;

        // Allow one render cycle for apply_relation_outcome to be processed.
        std::thread::sleep(Duration::from_millis(200));

        // The top message in the materialized tail viewport must remain
        // visible after the relation badge grows the seed message.
        driver.wait_for_screen(
            "filler-11 visible after relation refresh",
            ctx.timeout,
            |screen| screen.contains("filler-11"),
        )?;

        // The bottom of the original tail should still be visible too; this
        // keeps the assertion from passing on a jump to an unrelated viewport.
        driver.wait_for_screen(
            "filler-00 visible after relation refresh",
            ctx.timeout,
            |screen| screen.contains("filler-00"),
        )?;

        // The seed is newest in the API page and remains the tail anchor.
        driver.wait_for_screen(
            "seed message visible after relation refresh",
            ctx.timeout,
            |screen| screen.contains("smoke seed"),
        )?;

        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    stub.stop().await;
    outcome
}

/// `room_sort_filter_surface`: against the single-room stub, exercises the
/// sort/filter command + key-chord surface (ADR 0042). Asserts on the status
/// line the TUI writes for each change and on the live name-filter input, none
/// of which depend on having multiple rooms.
pub async fn room_sort_filter_surface(ctx: &Ctx) -> ScenarioOutcome {
    let stub = match Stub::start(&ctx.run_id).await {
        Ok(stub) => stub,
        Err(err) => return failed_before_spawn(err),
    };
    let mut driver = match ctx.spawn_tui("room_sort_filter_surface", &stub.base_url()) {
        Ok(driver) => driver,
        Err(err) => {
            stub.stop().await;
            return failed_before_spawn(err);
        }
    };

    let result = (|| {
        wait_first_paint(&mut driver, &stub.state, ctx.timeout)?;
        let room = stub.state.room_name.clone();

        // Sort commands report the active mode in the status line.
        submit_command(&mut driver, "/sort oldest")?;
        wait_for_text(&driver, "sort: Oldest", ctx.timeout)?;
        submit_command(&mut driver, "/sort recent")?;
        wait_for_text(&driver, "sort: Recent", ctx.timeout)?;

        // The cycle chord (Alt-S) advances Recent -> Oldest.
        press_alt(&mut driver, 's')?;
        wait_for_text(&driver, "sort: Oldest", ctx.timeout)?;

        // Filter commands likewise. The stub room is named, so it is a group and
        // remains visible under the Groups filter.
        submit_command(&mut driver, "/filter groups")?;
        wait_for_text(&driver, "filter: Groups", ctx.timeout)?;
        wait_for_text(&driver, &room, ctx.timeout)?;

        // The cycle chord (Alt-F) advances from the current filter.
        submit_command(&mut driver, "/filter all")?;
        wait_for_text(&driver, "filter: All", ctx.timeout)?;
        press_alt(&mut driver, 'f')?;
        wait_for_text(&driver, "filter: DMs", ctx.timeout)?;

        // The Alt-/ live name filter enters an input mode and narrows as typed;
        // a matching substring keeps the room visible.
        press_alt(&mut driver, '/')?;
        driver.wait_for_screen("name-filter input to open", ctx.timeout, |screen| {
            screen.contains("Filter:") || screen.contains("Room filter")
        })?;
        driver.type_text("Smoke")?;
        wait_for_text(&driver, &room, ctx.timeout)?;
        press_escape(&mut driver)?;

        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    stub.stop().await;
    outcome
}

pub async fn true_local_launch(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_launch",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let result = (|| {
        wait_for_room_and_input(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

pub async fn true_local_send_round_trip(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_send_round_trip",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let marker = format!("true-local-{}", &ctx.run_id[..8]);
    let result = (|| {
        wait_for_room_and_input(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        switch_room(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        driver.type_text(&marker)?;
        driver.press_enter()?;
        driver.wait_for_screen("the sent message to render", ctx.timeout, |screen| {
            screen.contains(&marker)
        })?;
        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

pub async fn true_local_command_surfaces(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_command_surfaces",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let result = (|| {
        wait_for_room_and_input(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        switch_room(&mut driver, &stack.rooms.general.name, ctx.timeout)?;

        submit_command(&mut driver, "/help")?;
        driver.wait_for_screen("help popup to render", ctx.timeout, |screen| {
            screen.contains("Help") && screen.contains("plain text")
        })?;
        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

pub async fn true_local_shortcuts_popup(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_shortcuts_popup",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let result = (|| {
        wait_for_room_and_input(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        submit_command(&mut driver, "/shortcuts")?;
        driver.wait_for_screen("shortcuts popup to render", ctx.timeout, |screen| {
            screen.contains("Shortcuts") && (screen.contains("jump") || screen.contains("quit"))
        })?;
        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

pub async fn true_local_status_commands(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_status_commands",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let result = (|| {
        wait_for_room_and_input(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        switch_room(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        submit_command(&mut driver, "/whereami")?;
        driver.wait_for_screen("whereami output to render", ctx.timeout, |screen| {
            screen.contains("Smoke General")
        })?;

        submit_command(&mut driver, "/whoami")?;
        driver.wait_for_screen("whoami output to render", ctx.timeout, |screen| {
            screen.contains("@axon-") || screen.contains("axon-")
        })?;

        submit_command(&mut driver, "/status")?;
        driver.wait_for_screen("status popup to render", ctx.timeout, |screen| {
            screen.contains("Status") || screen.contains("account") || screen.contains("server")
        })?;
        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

pub async fn true_local_room_navigation(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_room_navigation",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let result = (|| {
        wait_for_room_and_input(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        switch_room(&mut driver, &stack.rooms.relations.name, ctx.timeout)?;
        driver.wait_for_screen("relations seed to render", ctx.timeout, |screen| {
            screen.contains("relations")
        })?;
        switch_room(&mut driver, &stack.rooms.long_timeline.name, ctx.timeout)?;
        driver.wait_for_screen("timeline fixture to render", ctx.timeout, |screen| {
            screen.contains("jump fixture")
        })?;
        submit_command(&mut driver, "/refresh")?;
        driver.wait_for_screen("rooms still render after refresh", ctx.timeout, |screen| {
            screen.contains("Rooms") && screen.contains(&stack.rooms.general.name)
        })?;
        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

pub async fn true_local_send_variants(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_send_variants",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let short = &ctx.run_id[..8];
    let literal = format!("literal-{short}");
    let slash = format!("/slash-{short}");
    let html = format!("html-{short}");
    let rainbow = format!("rainbow-{short}");
    let result = (|| {
        wait_for_room_and_input(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        switch_room(&mut driver, &stack.rooms.general.name, ctx.timeout)?;

        submit_command(&mut driver, &format!("/literal {literal}"))?;
        wait_for_text(&driver, &literal, ctx.timeout)?;

        submit_command(&mut driver, &format!("//{}", slash.trim_start_matches('/')))?;
        wait_for_text(&driver, &slash, ctx.timeout)?;

        submit_command(&mut driver, &format!("/html <strong>{html}</strong>"))?;
        wait_for_text(&driver, &html, ctx.timeout)?;

        submit_command(&mut driver, &format!("/rainbow {rainbow}"))?;
        wait_for_text(&driver, &rainbow, ctx.timeout)?;

        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

pub async fn true_local_relations_render(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_relations_render",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let fixtures = &stack.fixtures.relations;
    let result = (|| {
        wait_for_room_and_input(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        switch_room(&mut driver, &stack.rooms.relations.name, ctx.timeout)?;

        driver.wait_for_screen("edited relation root to render", ctx.timeout, |screen| {
            screen.contains("relations root edited")
        })?;
        driver.wait_for_screen("reply relation to render", ctx.timeout, |screen| {
            screen.contains("relations reply message")
        })?;
        driver.wait_for_screen("formatted message to render", ctx.timeout, |screen| {
            screen.contains("formatted") && screen.contains("link")
        })?;
        driver.wait_for_screen("reaction badge to render", ctx.timeout, |screen| {
            screen.contains('👍') || screen.contains('✅')
        })?;

        submit_command(
            &mut driver,
            &format!("/event {}", fixtures.formatted_event_id),
        )?;
        driver.wait_for_screen(
            "event popup/status to include event body",
            ctx.timeout,
            |screen| {
                screen.contains("formatted bold link")
                    || screen.contains(&fixtures.formatted_event_id)
            },
        )?;

        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

pub async fn true_local_react(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_react",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let marker = format!("react-target-{}", &ctx.run_id[..8]);
    let result = (|| {
        wait_for_room_and_input(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        switch_room(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        driver.type_text(&marker)?;
        driver.press_enter()?;
        wait_for_text(&driver, &marker, ctx.timeout)?;

        submit_command(&mut driver, "/react 🚀")?;
        driver.wait_for_screen("reaction command to complete", ctx.timeout, |screen| {
            screen.contains("sent") || screen.contains('🚀')
        })?;

        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

pub async fn true_local_thread_panel(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_thread_panel",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let result = (|| {
        wait_for_room_and_input(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        switch_room(&mut driver, &stack.rooms.relations.name, ctx.timeout)?;
        driver.wait_for_screen("thread fixture to render", ctx.timeout, |screen| {
            screen.contains("relations thread root") || screen.contains("relations thread member")
        })?;
        submit_command(&mut driver, "/thread")?;
        driver.wait_for_screen("thread panel to render", ctx.timeout, |screen| {
            screen.contains("Thread") || screen.contains("relations thread member")
        })?;
        press_escape(&mut driver)?;
        driver.wait_for_screen(
            "main relations room after closing thread",
            ctx.timeout,
            |screen| screen.contains(&stack.rooms.relations.name),
        )?;
        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

pub async fn true_local_jump_to_date(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let Some(date) = stack.rooms.long_timeline.jump_dates.get(1) else {
        return failed_before_spawn(anyhow!("true-local manifest has no jump date"));
    };
    let room_name = &stack.rooms.long_timeline.name;
    let expected = format!("jump fixture {date}");
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_jump_to_date",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let result = (|| {
        wait_for_room_and_input(&mut driver, &stack.rooms.general.name, ctx.timeout)?;
        driver.type_text(&format!("/room {room_name}"))?;
        driver.press_enter()?;
        driver.wait_for_screen("the long timeline room to render", ctx.timeout, |screen| {
            screen.contains(room_name)
        })?;
        driver.type_text(&format!("/jump {date}"))?;
        driver.press_enter()?;
        driver.wait_for_screen("jump-date messages to render", ctx.timeout, |screen| {
            screen.contains(&expected)
        })?;
        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

/// `true_local_room_sort`: against the 3-room true-local stack, drives the sort
/// modes via `/sort` and the Alt-S cycle (ADR 0042). Sorting never hides rooms,
/// so all three named rooms stay visible throughout.
pub async fn true_local_room_sort(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_room_sort",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let general = &stack.rooms.general.name;
    let timeline = &stack.rooms.long_timeline.name;
    let relations = &stack.rooms.relations.name;
    let result = (|| {
        wait_for_room_and_input(&mut driver, general, ctx.timeout)?;

        let all_three = |screen: &str| {
            screen.contains(general) && screen.contains(timeline) && screen.contains(relations)
        };

        submit_command(&mut driver, "/sort oldest")?;
        wait_for_text(&driver, "sort: Oldest", ctx.timeout)?;
        driver.wait_for_screen(
            "all rooms visible under oldest sort",
            ctx.timeout,
            all_three,
        )?;

        // Alphabetical (A–Z): assert the status surfaced and rooms still render.
        submit_command(&mut driver, "/sort az")?;
        wait_for_text(&driver, "sort: A", ctx.timeout)?;
        driver.wait_for_screen("all rooms visible under A–Z sort", ctx.timeout, all_three)?;

        submit_command(&mut driver, "/sort recent")?;
        wait_for_text(&driver, "sort: Recent", ctx.timeout)?;

        // Cycle chord advances Recent -> Oldest.
        press_alt(&mut driver, 's')?;
        wait_for_text(&driver, "sort: Oldest", ctx.timeout)?;
        driver.wait_for_screen("all rooms visible after cycle", ctx.timeout, all_three)?;

        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

/// `true_local_room_filter`: against the 3-room true-local stack, drives the
/// filter modes (ADR 0042). All three rooms are named groups (no DM fixtures),
/// so the Groups filter keeps them and the DMs filter hides the unselected ones.
/// Also covers favourites (after `/pin`) and the Alt-/ live name filter.
pub async fn true_local_room_filter(ctx: &Ctx) -> ScenarioOutcome {
    let stack = match ctx.local_stack.as_ref() {
        Some(stack) => stack,
        None => return failed_before_spawn(anyhow!("true-local stack manifest missing")),
    };
    let mut driver = match ctx.spawn_tui_with_token(
        "true_local_room_filter",
        &stack.axon_base_url,
        Some(&stack.axon_bearer_token),
    ) {
        Ok(driver) => driver,
        Err(err) => return failed_before_spawn(err),
    };

    let general = &stack.rooms.general.name;
    let timeline = &stack.rooms.long_timeline.name;
    let relations = &stack.rooms.relations.name;
    let result = (|| {
        wait_for_room_and_input(&mut driver, general, ctx.timeout)?;
        // Select General deterministically so the "keep the selected room
        // visible" rule applies to a known room in the negative assertions below.
        switch_room(&mut driver, general, ctx.timeout)?;

        // Groups: every named room stays visible.
        submit_command(&mut driver, "/filter groups")?;
        wait_for_text(&driver, "filter: Groups", ctx.timeout)?;
        driver.wait_for_screen("all rooms visible under groups", ctx.timeout, |screen| {
            screen.contains(general) && screen.contains(timeline) && screen.contains(relations)
        })?;

        // DMs: the unselected named rooms drop out (heuristic excludes them);
        // the selected General stays by the keep-selected rule.
        submit_command(&mut driver, "/filter dms")?;
        wait_for_text(&driver, "filter: DMs", ctx.timeout)?;
        driver.wait_for_screen("group rooms hidden under DMs", ctx.timeout, |screen| {
            !screen.contains(timeline) && !screen.contains(relations)
        })?;

        // All: everything returns.
        submit_command(&mut driver, "/filter all")?;
        wait_for_text(&driver, "filter: All", ctx.timeout)?;
        driver.wait_for_screen("all rooms visible again", ctx.timeout, |screen| {
            screen.contains(timeline) && screen.contains(relations)
        })?;

        // Favourites: pin the selected General, then only pinned rooms remain.
        submit_command(&mut driver, "/pin")?;
        submit_command(&mut driver, "/filter fav")?;
        wait_for_text(&driver, "filter: Favorites", ctx.timeout)?;
        driver.wait_for_screen(
            "only the pinned room under favourites",
            ctx.timeout,
            |screen| {
                screen.contains(general)
                    && !screen.contains(timeline)
                    && !screen.contains(relations)
            },
        )?;

        // Live name filter (Alt-/): narrows to a typed substring.
        submit_command(&mut driver, "/filter all")?;
        press_alt(&mut driver, '/')?;
        driver.wait_for_screen("name-filter input to open", ctx.timeout, |screen| {
            screen.contains("Filter:") || screen.contains("Room filter")
        })?;
        driver.type_text(timeline)?;
        driver.wait_for_screen(
            "name filter matches the timeline room",
            ctx.timeout,
            |screen| screen.contains(timeline),
        )?;
        press_escape(&mut driver)?;

        driver.terminate();
        Ok(())
    })();

    let outcome = ScenarioOutcome::capture(&driver, result);
    drop(driver);
    outcome
}

fn submit_command(driver: &mut PtyDriver, command: &str) -> anyhow::Result<()> {
    driver.type_text(command)?;
    driver.press_enter()
}

fn press_escape(driver: &mut PtyDriver) -> anyhow::Result<()> {
    driver.send_bytes(&[0x1b])
}

/// Send an `Alt`+<char> chord. Terminals encode this as ESC followed by the
/// character byte in one write, which crossterm parses as `KeyModifiers::ALT`.
fn press_alt(driver: &mut PtyDriver, ch: char) -> anyhow::Result<()> {
    driver.send_bytes(&[0x1b, ch as u8])
}

fn switch_room(driver: &mut PtyDriver, room_name: &str, timeout: Duration) -> anyhow::Result<()> {
    submit_command(driver, &format!("/room {room_name}"))?;
    driver.wait_for_screen("room switch to render", timeout, |screen| {
        screen.contains(room_name)
    })
}

fn wait_for_text(driver: &PtyDriver, text: &str, timeout: Duration) -> anyhow::Result<()> {
    driver.wait_for_screen("text to render", timeout, |screen| screen.contains(text))
}

fn failed_before_spawn(err: anyhow::Error) -> ScenarioOutcome {
    ScenarioOutcome {
        result: Err(err),
        transcript: Vec::new(),
        final_screen: String::new(),
    }
}
