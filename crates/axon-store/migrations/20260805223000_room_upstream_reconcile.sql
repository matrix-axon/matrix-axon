-- Rooms the homeserver appears to have stopped knowing about (ADR 0090).
--
-- A room purged upstream (a deleted bridge portal, an admin room purge) gives
-- Axon no signal at all: sync simply stops mentioning it, no leave and no
-- tombstone arrive, and the local state store keeps reporting the account as
-- joined. Everything derived from that room then freezes at whatever it last
-- held — including the ADR 0070 unread count, which sits nonzero forever
-- because nothing ever recomputes it and no read receipt can land on a room
-- the homeserver will not accept one for.
--
-- This table is the durable half of the two-step reconcile that fixes it, so a
-- crash between the two steps loses nothing and the next boot resumes:
--
--   'suspect' — a room-scoped call was definitively rejected upstream for a
--               room the account believes it is joined to. Cheap to record (no
--               network), deliberately not acted on: one rejection is not proof,
--               and the rejecting call has a latency budget to keep.
--   'gone'    — a later, bounded probe confirmed the homeserver does not serve
--               this room. The unread watcher pins the room's counts to zero
--               and stops re-deriving them.
--
-- Nothing here deletes room content: `events`, `room_state`, and the room's
-- place in the room list are untouched, so history stays readable and a room
-- that comes back (a restored purge, a re-created portal) recovers by having
-- its row dropped.
CREATE TABLE room_upstream_reconcile (
    account_id       UUID        NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    room_id          TEXT        NOT NULL,
    state            TEXT        NOT NULL CHECK (state IN ('suspect', 'gone')),
    -- The upstream rejection that flagged this room, for debugging a wrong
    -- verdict after the fact. Never holds a secret: it is an error kind plus
    -- message from a homeserver response.
    detail           TEXT,
    first_flagged_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, room_id)
);

-- The reconcile sweep reads only `suspect` rows, and there are normally none.
CREATE INDEX room_upstream_reconcile_suspect_idx
    ON room_upstream_reconcile (account_id)
    WHERE state = 'suspect';

-- Reuse the shared trigger from the accounts migration.
CREATE TRIGGER room_upstream_reconcile_set_updated_at
    BEFORE UPDATE ON room_upstream_reconcile
    FOR EACH ROW EXECUTE FUNCTION trigger_set_updated_at();
