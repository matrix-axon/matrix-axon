-- SDK-derived pending-invite projection (issue #279, ADR 0091).
--
-- An invited room has no rows in `events` and must not be written into
-- `room_state` (stripped invite_state usually lacks event_id / origin_ts).
-- This table is the durable current-value view a `GET /v1/invites` load
-- reads. matrix-sdk remains the source of truth; axon-sync's watcher
-- upserts and prunes.
--
-- `invited_at` is first-seen (DEFAULT now() on insert only). The upsert
-- must not bump it when display fields refresh. `updated_at` is the usual
-- trigger-maintained column.

CREATE TABLE room_invites (
    account_id              UUID        NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    room_id                 TEXT        NOT NULL,
    name                    TEXT,
    avatar_url              TEXT,
    topic                   TEXT,
    canonical_alias         TEXT,
    room_type               TEXT,
    inviter_user_id         TEXT        NOT NULL,
    inviter_display_name    TEXT,
    is_direct               BOOLEAN     NOT NULL DEFAULT false,
    encrypted               BOOLEAN     NOT NULL DEFAULT false,
    invited_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, room_id)
);

CREATE TRIGGER room_invites_set_updated_at
    BEFORE UPDATE ON room_invites
    FOR EACH ROW EXECUTE FUNCTION trigger_set_updated_at();
