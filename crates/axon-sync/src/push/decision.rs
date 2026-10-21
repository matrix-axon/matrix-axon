//! The ADR 0113 case list, as a pure function.
//!
//! The router stores `after` only after this returns. The first matching case
//! wins. A case that sends or releases also drops an unpaired rise.

use super::message::{PushClass, PushMessage};

/// One room's in-memory push memory. The slot and the unpaired rise are not durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoomState {
    pub seeded: bool,
    pub pair: (u64, u64),
    pub slot: Option<PushMessage>,
    pub unpaired: Option<(u64, u64)>,
}

impl RoomState {
    pub(crate) fn unseeded(pair: (u64, u64)) -> Self {
        Self {
            seeded: false,
            pair,
            slot: None,
            unpaired: None,
        }
    }
}

/// What one observation did. `unknown_ids` are logged by the router, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    pub state: RoomState,
    pub send: Option<PushMessage>,
    pub unknown_ids: Vec<String>,
}

/// Apply one observation.
///
/// `after` is `None` when the observation is a message and the unread watcher
/// has not published a count. A seeded room then keeps its pair. An unseeded
/// room stays unseeded and may hold a notifying message. The later count that
/// seeds the room does not send, and it keeps that message when it has none
/// of its own.
pub(crate) fn decide(
    state: &RoomState,
    after: Option<(u64, u64)>,
    messages: &[PushMessage],
) -> Decision {
    let unknown_ids = messages
        .iter()
        .filter(|message| message.class == PushClass::Unknown)
        .map(|message| message.event_id.clone())
        .collect();
    let notifying = latest(messages, PushClass::Notify);
    let unknown = latest(messages, PushClass::Unknown);

    if !state.seeded {
        let Some(after) = after else {
            let mut kept = RoomState::unseeded(state.pair);
            kept.slot = notifying.or(state.slot.clone());
            return Decision {
                state: kept,
                send: None,
                unknown_ids,
            };
        };
        let mut seeded = RoomState::unseeded(after);
        seeded.seeded = true;
        // A count-only seed must not drop the message slotted while the room
        // was still unseeded. That message is the first observation's slot.
        seeded.slot = notifying.or_else(|| state.slot.clone());
        return Decision {
            state: seeded,
            send: None,
            unknown_ids,
        };
    }

    let after = after.unwrap_or(state.pair);
    let mut slot = state.slot.clone();
    let mut unpaired = state.unpaired;
    let mut send = None;
    let rose = after.0 > state.pair.0 || after.1 > state.pair.1;
    let fell = !rose && (after.0 < state.pair.0 || after.1 < state.pair.1);

    if rose && notifying.is_some() {
        send = notifying;
        slot = None;
        unpaired = None;
    } else if rose && slot.is_some() {
        send = slot.take();
        unpaired = None;
    } else if rose && unknown.is_some() {
        send = unknown;
        unpaired = None;
    } else if rose {
        unpaired = Some(after);
    } else if !rose && !fell && unpaired == Some(after) && notifying.is_some() {
        send = notifying;
        slot = None;
        unpaired = None;
    } else if !rose && !fell && notifying.is_some() {
        slot = notifying;
    } else if fell && after.0 == 0 {
        slot = None;
        unpaired = None;
    } else if fell && after.0 > 0 && notifying.is_some() {
        send = notifying;
        slot = None;
        unpaired = None;
    }

    Decision {
        state: RoomState {
            seeded: true,
            pair: after,
            slot,
            unpaired,
        },
        send,
        unknown_ids,
    }
}

fn latest(messages: &[PushMessage], class: PushClass) -> Option<PushMessage> {
    messages
        .iter()
        .rev()
        .find(|message| message.class == class)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, class: PushClass) -> PushMessage {
        PushMessage {
            event_id: id.to_owned(),
            sender: "@bob:example".to_owned(),
            body: "hello".to_owned(),
            thread_root_id: None,
            class,
        }
    }

    fn seeded(pair: (u64, u64)) -> RoomState {
        RoomState {
            seeded: true,
            pair,
            slot: None,
            unpaired: None,
        }
    }

    #[test]
    fn the_first_observation_seeds_and_does_not_send() {
        let state = RoomState::unseeded((1, 0));
        let decision = decide(&state, Some((5, 1)), &[message("m", PushClass::Notify)]);
        assert!(decision.send.is_none());
        assert!(decision.state.seeded);
        assert_eq!(decision.state.pair, (5, 1));
        assert_eq!(decision.state.slot.unwrap().event_id, "m");
    }

    #[test]
    fn a_rise_with_a_notifying_message_sends_the_latest_and_clears_the_slot() {
        let mut state = seeded((1, 0));
        state.slot = Some(message("old", PushClass::Notify));
        let decision = decide(
            &state,
            Some((2, 0)),
            &[
                message("a", PushClass::Notify),
                message("b", PushClass::Notify),
            ],
        );
        assert_eq!(decision.send.unwrap().event_id, "b");
        assert!(decision.state.slot.is_none());
        assert_eq!(decision.state.pair, (2, 0));
    }

    #[test]
    fn one_count_rising_is_still_a_rise_when_the_other_falls() {
        let state = seeded((3, 2));
        let decision = decide(&state, Some((4, 0)), &[message("m", PushClass::Notify)]);
        assert!(decision.send.is_some());
    }

    #[test]
    fn a_rise_with_an_occupied_slot_and_no_notifying_message_releases_the_slot() {
        let mut state = seeded((1, 0));
        state.slot = Some(message("held", PushClass::Notify));
        let decision = decide(&state, Some((2, 0)), &[message("unk", PushClass::Unknown)]);
        assert_eq!(decision.send.unwrap().event_id, "held");
        assert!(decision.state.slot.is_none());
        assert!(decision.state.unpaired.is_none());
        assert_eq!(decision.unknown_ids, vec!["unk".to_owned()]);
    }

    #[test]
    fn an_unknown_message_sends_only_on_a_rise_with_an_empty_slot() {
        let state = seeded((1, 0));
        let decision = decide(&state, Some((2, 0)), &[message("unk", PushClass::Unknown)]);
        assert_eq!(decision.send.unwrap().event_id, "unk");
        assert!(decision.state.slot.is_none());

        let mut occupied = seeded((1, 0));
        occupied.slot = Some(message("held", PushClass::Notify));
        let fell = decide(
            &occupied,
            Some((0, 0)),
            &[message("unk", PushClass::Unknown)],
        );
        assert!(fell.send.is_none());
        assert!(fell.state.slot.is_none());
    }

    #[test]
    fn a_rise_with_nothing_to_send_is_remembered_until_a_notifying_message_claims_it() {
        let state = seeded((1, 0));
        let remembered = decide(&state, Some((2, 0)), &[message("quiet", PushClass::Silent)]);
        assert!(remembered.send.is_none());
        assert_eq!(remembered.state.unpaired, Some((2, 0)));
        assert!(remembered.state.slot.is_none());

        let claimed = decide(
            &remembered.state,
            Some((2, 0)),
            &[message("now", PushClass::Notify)],
        );
        assert_eq!(claimed.send.unwrap().event_id, "now");
        assert!(claimed.state.unpaired.is_none());
        assert!(claimed.state.slot.is_none());
    }

    #[test]
    fn an_unknown_message_does_not_claim_an_unpaired_rise() {
        let mut state = seeded((2, 0));
        state.unpaired = Some((2, 0));
        let decision = decide(&state, Some((2, 0)), &[message("unk", PushClass::Unknown)]);
        assert!(decision.send.is_none());
        assert_eq!(decision.state.unpaired, Some((2, 0)));
    }

    #[test]
    fn an_equal_count_with_a_notifying_message_fills_the_slot() {
        let state = seeded((2, 0));
        let decision = decide(&state, Some((2, 0)), &[message("m", PushClass::Notify)]);
        assert!(decision.send.is_none());
        assert_eq!(decision.state.slot.unwrap().event_id, "m");
    }

    #[test]
    fn a_fall_to_zero_clears_without_sending() {
        let mut state = seeded((2, 1));
        state.slot = Some(message("held", PushClass::Notify));
        state.unpaired = Some((2, 1));
        let decision = decide(&state, Some((0, 0)), &[message("m", PushClass::Notify)]);
        assert!(decision.send.is_none());
        assert!(decision.state.slot.is_none());
        assert!(decision.state.unpaired.is_none());
        assert_eq!(decision.state.pair, (0, 0));
    }

    #[test]
    fn a_fall_that_still_leaves_unread_sends_a_notifying_message() {
        let mut state = seeded((4, 1));
        state.slot = Some(message("old", PushClass::Notify));
        let decision = decide(&state, Some((3, 0)), &[message("m", PushClass::Notify)]);
        assert_eq!(decision.send.unwrap().event_id, "m");
        assert!(decision.state.slot.is_none());
        assert_eq!(decision.state.pair, (3, 0));
    }

    #[test]
    fn a_partial_read_without_a_notifying_message_keeps_the_slot() {
        let mut state = seeded((4, 1));
        state.slot = Some(message("held", PushClass::Notify));
        state.unpaired = Some((4, 1));
        let decision = decide(&state, Some((3, 1)), &[]);
        assert!(decision.send.is_none());
        assert_eq!(decision.state.slot.unwrap().event_id, "held");
        assert_eq!(decision.state.unpaired, Some((4, 1)));
        assert_eq!(decision.state.pair, (3, 1));
    }

    #[test]
    fn a_silent_message_does_not_fill_or_clear_the_slot() {
        let mut state = seeded((2, 0));
        state.slot = Some(message("held", PushClass::Notify));
        let decision = decide(&state, Some((2, 0)), &[message("s", PushClass::Silent)]);
        assert!(decision.send.is_none());
        assert_eq!(decision.state.slot.unwrap().event_id, "held");
        assert!(decision.state.unpaired.is_none());
    }

    #[test]
    fn a_message_slotted_before_the_seed_survives_the_seed() {
        let state = RoomState::unseeded((0, 0));
        let slotted = decide(&state, None, &[message("m", PushClass::Notify)]);
        assert!(slotted.send.is_none());
        assert!(!slotted.state.seeded);
        assert_eq!(slotted.state.slot.as_ref().unwrap().event_id, "m");

        let seeded = decide(&slotted.state, Some((3, 0)), &[]);
        assert!(seeded.send.is_none());
        assert!(seeded.state.seeded);
        assert_eq!(seeded.state.pair, (3, 0));
        assert_eq!(seeded.state.slot.unwrap().event_id, "m");
    }

    #[test]
    fn an_unknown_count_on_a_seeded_room_is_treated_as_equal() {
        let state = seeded((2, 0));
        let decision = decide(&state, None, &[message("m", PushClass::Notify)]);
        assert!(decision.send.is_none());
        assert_eq!(decision.state.pair, (2, 0));
        assert_eq!(decision.state.slot.unwrap().event_id, "m");
    }
}
