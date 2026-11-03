-- Room detail acquisition (ADR 0126 decisions 2-4).
--
-- `membership_epoch` fences acquisition results against membership changes.
-- Values come from a sequence rather than a per-row counter: a purge rebuild
-- deletes and re-inserts summary rows, and a counter restarting at its default
-- could hand a rejoined room the epoch an in-flight job recorded before the
-- leave. Existing rows share 0; only a change of value matters.
CREATE SEQUENCE room_membership_epoch_seq;
ALTER TABLE room_summaries ADD COLUMN membership_epoch BIGINT NOT NULL DEFAULT 0;
ALTER TABLE room_summaries
    ALTER COLUMN membership_epoch SET DEFAULT nextval('room_membership_epoch_seq');

-- Leave and rejoin transitions start a new epoch, which rejects any result
-- whose job started before them, even after a rejoin.
CREATE FUNCTION advance_room_membership_epoch() RETURNS TRIGGER AS $$
BEGIN
    NEW.membership_epoch := nextval('room_membership_epoch_seq');
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER room_summaries_advance_membership_epoch
    BEFORE UPDATE ON room_summaries
    FOR EACH ROW
    WHEN (NEW.hidden_left IS DISTINCT FROM OLD.hidden_left)
    EXECUTE FUNCTION advance_room_membership_epoch();

-- One row per account, room, and kind: the homeserver-local aliases, the
-- directory publication, and one absence confirmation per metadata state
-- type. Never merged into room_state. The summary foreign key deletes rows on
-- account removal and room purge; the trigger below deletes them on leave.
--
-- Times are Unix milliseconds from the PostgreSQL clock. `value_status`,
-- `value`, `fetched_at`, and `expires_at` are the last conclusive answer;
-- `failure`, `failures`, and `next_attempt_at` describe refresh attempts, so
-- a failed refresh never replaces a usable stale value.
CREATE TABLE room_detail_acquisitions (
    account_id      UUID    NOT NULL,
    room_id         TEXT    NOT NULL,
    kind            TEXT    NOT NULL
        CHECK (kind IN ('local_aliases', 'directory_visibility', 'absence')),
    -- The metadata state type an absence row confirms; '' for the others.
    state_type      TEXT    NOT NULL DEFAULT '',
    value_status    TEXT
        CHECK (value_status IN ('available', 'partial', 'too_large', 'forbidden',
                                'not_found', 'unsupported', 'absent')),
    value           JSONB,
    fetched_at      BIGINT,
    expires_at      BIGINT,
    failure         TEXT
        CHECK (failure IN ('timeout', 'server_error', 'rate_limited', 'transport',
                           'unauthorized', 'invalid_response')),
    failures        INTEGER NOT NULL DEFAULT 0 CHECK (failures >= 0),
    next_attempt_at BIGINT,
    PRIMARY KEY (account_id, room_id, kind, state_type),
    FOREIGN KEY (account_id, room_id)
        REFERENCES room_summaries (account_id, room_id) ON DELETE CASCADE,
    CHECK ((kind = 'absence') = (state_type <> '')),
    CHECK (state_type IN ('', 'm.room.canonical_alias', 'm.room.create',
                          'm.room.join_rules', 'm.room.encryption',
                          'm.room.power_levels', 'm.room.server_acl',
                          'm.room.history_visibility', 'm.room.guest_access')),
    CHECK ((value_status IS NULL) = (fetched_at IS NULL)
           AND (fetched_at IS NULL) = (expires_at IS NULL)),
    CHECK (value_status IS NULL OR CASE WHEN kind = 'absence'
        THEN value_status IN ('absent', 'forbidden')
        ELSE value_status <> 'absent' END),
    CHECK ((failure IS NULL) = (failures = 0))
);

-- A local leave removes every cached detail and confirmation, so a removal
-- is never served from cache and a rejoin starts empty.
CREATE FUNCTION delete_left_room_detail_acquisitions() RETURNS TRIGGER AS $$
BEGIN
    DELETE FROM room_detail_acquisitions
    WHERE account_id = NEW.account_id AND room_id = NEW.room_id;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER room_summaries_delete_left_acquisitions
    AFTER UPDATE ON room_summaries
    FOR EACH ROW
    WHEN (NEW.hidden_left AND NOT OLD.hidden_left)
    EXECUTE FUNCTION delete_left_room_detail_acquisitions();

-- Serializes a tuple's insert with an absence confirmation of the same type.
-- Under READ COMMITTED neither can see the other's uncommitted row, so without
-- it both could commit: the tuple's delete finds no confirmation yet, and the
-- confirmation's check finds no tuple yet. Whichever takes the lock second
-- runs its next statement on a snapshot that includes the first's commit.
CREATE FUNCTION lock_room_absence(p_account_id UUID, p_room_id TEXT, p_state_type TEXT)
RETURNS VOID AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended(p_account_id::text || '/' || p_room_id || '/' || p_state_type, 0));
END;
$$ LANGUAGE plpgsql;

-- A tuple of a confirmed-absent type supersedes the confirmation. Reads also
-- ignore a confirmation beside a tuple, as a second line of defence.
CREATE FUNCTION delete_superseded_absence() RETURNS TRIGGER AS $$
BEGIN
    PERFORM lock_room_absence(NEW.account_id, NEW.room_id, NEW.event_type);
    DELETE FROM room_detail_acquisitions
    WHERE account_id = NEW.account_id AND room_id = NEW.room_id
      AND kind = 'absence' AND state_type = NEW.event_type;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER room_state_delete_superseded_absence
    AFTER INSERT ON room_state
    FOR EACH ROW
    WHEN (NEW.state_key = '' AND NEW.event_type IN (
        'm.room.canonical_alias', 'm.room.create', 'm.room.join_rules',
        'm.room.encryption', 'm.room.power_levels', 'm.room.server_acl',
        'm.room.history_visibility', 'm.room.guest_access'))
    EXECUTE FUNCTION delete_superseded_absence();
