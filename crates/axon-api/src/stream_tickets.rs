//! Media stream tickets (ADR 0139 section 5): short-lived URLs that stand in
//! for a bearer token for one video or audio object, since a `<video>` element
//! cannot send an `Authorization` header.
//!
//! The table is in memory: a restart forgets every ticket, and the client asks
//! for a new one. A ticket only names what to serve and who asked. Whether it
//! may still be served is decided on every use by the route, which re-checks
//! the issuing token and re-runs the media route's own event checks.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use uuid::Uuid;

/// A ticket expires this long after its last use.
pub const TICKET_IDLE: Duration = Duration::from_secs(30 * 60);
/// A ticket expires this long after it was issued, however often it is used.
pub const TICKET_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
/// The most tickets the table holds. Past it the least recently used goes.
pub const MAX_TICKETS: usize = 4096;
/// The most live tickets one issuing token holds. Its next one evicts its own
/// least recently used ticket, so a client minting in a loop churns only its
/// own tickets and never pushes other users' out of the shared table.
pub const MAX_TICKETS_PER_TOKEN: usize = 32;

/// What a ticket was issued for. Carries no event id: the route resolves the
/// event from the MXC URI on every use, exactly as the bearer media route does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamTicket {
    /// The account whose homeserver connection serves the object.
    pub account_id: Uuid,
    /// The object, as an `mxc://` URI.
    pub mxc_url: String,
    /// The bearer token that issued the ticket, re-checked on every use.
    pub token_id: Uuid,
    /// That token's credential session, for the log.
    pub session_id: Uuid,
}

/// The live tickets, keyed by the ticket itself.
///
/// Deliberately not `Debug`: its keys are credentials.
#[derive(Default)]
pub struct StreamTickets {
    state: Mutex<TicketState>,
}

#[derive(Default)]
struct TicketState {
    entries: HashMap<String, Entry>,
    /// Bumped on every issue and use, to order entries by recency.
    clock: u64,
}

struct Entry {
    ticket: StreamTicket,
    issued_at: Instant,
    used_at: Instant,
    used: u64,
}

impl Entry {
    fn live_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.used_at) < TICKET_IDLE
            && now.saturating_duration_since(self.issued_at) < TICKET_LIFETIME
    }
}

impl StreamTickets {
    /// Issue a ticket for `ticket` and return it: 32 random bytes,
    /// base64url-encoded.
    pub fn issue(&self, ticket: StreamTicket) -> String {
        self.issue_at(ticket, Instant::now())
    }

    /// What `key` was issued for, if it is a live ticket. A use: it restarts
    /// the ticket's idle expiry and makes it the most recently used. An
    /// expired ticket is dropped.
    pub fn redeem(&self, key: &str) -> Option<StreamTicket> {
        self.redeem_at(key, Instant::now())
    }

    /// Drop a ticket the route found it can no longer serve, such as one whose
    /// issuing token was revoked.
    pub fn forget(&self, key: &str) {
        self.lock().entries.remove(key);
    }

    fn issue_at(&self, ticket: StreamTicket, now: Instant) -> String {
        let key = axon_core::generate_opaque_secret();
        let mut state = self.lock();
        state.entries.retain(|_, entry| entry.live_at(now));

        // The token's own cap first: a token at its cap replaces one of its
        // own tickets, so it never takes a slot from anyone else's.
        let token_id = ticket.token_id;
        let owned = state
            .entries
            .iter()
            .filter(|(_, entry)| entry.ticket.token_id == token_id);
        if owned.clone().count() >= MAX_TICKETS_PER_TOKEN {
            let oldest = owned
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                state.entries.remove(&oldest);
            }
        }
        if state.entries.len() >= MAX_TICKETS {
            let oldest = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                state.entries.remove(&oldest);
            }
        }

        state.clock += 1;
        let used = state.clock;
        state.entries.insert(
            key.clone(),
            Entry {
                ticket,
                issued_at: now,
                used_at: now,
                used,
            },
        );
        key
    }

    fn redeem_at(&self, key: &str, now: Instant) -> Option<StreamTicket> {
        let mut state = self.lock();
        state.clock += 1;
        let used = state.clock;
        let entry = state.entries.get_mut(key)?;
        if !entry.live_at(now) {
            state.entries.remove(key);
            return None;
        }
        entry.used_at = now;
        entry.used = used;
        Some(entry.ticket.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TicketState> {
        // Only map reads and writes happen under the lock: no I/O or await.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(token_id: Uuid) -> StreamTicket {
        StreamTicket {
            account_id: Uuid::new_v4(),
            mxc_url: format!("mxc://example.org/{}", Uuid::new_v4().simple()),
            token_id,
            session_id: token_id,
        }
    }

    #[test]
    fn a_ticket_is_32_random_bytes_base64url_encoded() {
        let tickets = StreamTickets::default();
        let token = Uuid::new_v4();
        let a = tickets.issue(ticket(token));
        let b = tickets.issue(ticket(token));
        assert_ne!(a, b);
        for key in [&a, &b] {
            let bytes = base64::Engine::decode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                key.as_bytes(),
            )
            .expect("base64url without padding");
            assert_eq!(bytes.len(), 32, "{key}");
        }
    }

    #[test]
    fn redeeming_returns_what_was_issued_and_nothing_else() {
        let tickets = StreamTickets::default();
        let issued = ticket(Uuid::new_v4());
        let key = tickets.issue(issued.clone());
        assert_eq!(tickets.redeem(&key), Some(issued));
        assert_eq!(tickets.redeem("not-a-ticket"), None);
        tickets.forget(&key);
        assert_eq!(tickets.redeem(&key), None);
    }

    #[test]
    fn a_ticket_expires_30_minutes_after_its_last_use() {
        let tickets = StreamTickets::default();
        let start = Instant::now();
        let issued = ticket(Uuid::new_v4());
        let key = tickets.issue_at(issued.clone(), start);

        // Each use restarts the idle clock.
        let used = start + TICKET_IDLE - Duration::from_secs(1);
        assert_eq!(tickets.redeem_at(&key, used), Some(issued));
        let still = used + TICKET_IDLE - Duration::from_secs(1);
        assert!(tickets.redeem_at(&key, still).is_some());

        // A full idle period after the last use, it is gone for good.
        assert_eq!(tickets.redeem_at(&key, still + TICKET_IDLE), None);
        assert_eq!(tickets.len(), 0, "an expired ticket is dropped");
        assert_eq!(tickets.redeem_at(&key, still), None);
    }

    #[test]
    fn a_ticket_expires_12_hours_after_issue_however_often_it_is_used() {
        let tickets = StreamTickets::default();
        let start = Instant::now();
        let key = tickets.issue_at(ticket(Uuid::new_v4()), start);
        let step = TICKET_IDLE / 2;
        let mut now = start;
        while now + step < start + TICKET_LIFETIME {
            now += step;
            assert!(tickets.redeem_at(&key, now).is_some(), "{now:?}");
        }
        assert_eq!(tickets.redeem_at(&key, start + TICKET_LIFETIME), None);
    }

    #[test]
    fn issuing_reclaims_expired_tickets() {
        let tickets = StreamTickets::default();
        let start = Instant::now();
        for _ in 0..3 {
            tickets.issue_at(ticket(Uuid::new_v4()), start);
        }
        tickets.issue_at(ticket(Uuid::new_v4()), start + TICKET_IDLE);
        assert_eq!(tickets.len(), 1);
    }

    #[test]
    fn a_token_past_its_cap_evicts_its_own_least_recently_used_ticket() {
        let tickets = StreamTickets::default();
        let now = Instant::now();
        let token = Uuid::new_v4();
        let other = tickets.issue_at(ticket(Uuid::new_v4()), now);
        let mine: Vec<String> = (0..MAX_TICKETS_PER_TOKEN)
            .map(|_| tickets.issue_at(ticket(token), now))
            .collect();
        // Use the oldest, so the second-oldest is now the least recent.
        assert!(tickets.redeem_at(&mine[0], now).is_some());

        tickets.issue_at(ticket(token), now);

        assert_eq!(tickets.len(), MAX_TICKETS_PER_TOKEN + 1);
        assert!(
            tickets.redeem_at(&mine[1], now).is_none(),
            "own LRU evicted"
        );
        assert!(
            tickets.redeem_at(&mine[0], now).is_some(),
            "recently used kept"
        );
        assert!(
            tickets.redeem_at(&other, now).is_some(),
            "another token's ticket is untouched, though it is older"
        );
    }

    #[test]
    fn a_full_table_evicts_its_least_recently_used_ticket() {
        let tickets = StreamTickets::default();
        let now = Instant::now();
        let keys: Vec<String> = (0..MAX_TICKETS)
            .map(|_| tickets.issue_at(ticket(Uuid::new_v4()), now))
            .collect();
        assert!(tickets.redeem_at(&keys[0], now).is_some());

        tickets.issue_at(ticket(Uuid::new_v4()), now);

        assert_eq!(tickets.len(), MAX_TICKETS);
        assert!(tickets.redeem_at(&keys[1], now).is_none(), "LRU evicted");
        assert!(
            tickets.redeem_at(&keys[0], now).is_some(),
            "recently used kept"
        );
    }
}
