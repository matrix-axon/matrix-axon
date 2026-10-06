-- Pay for the exact count once on upgrade, then maintain it transactionally.
-- Statement transition tables count only inserted rows (including ON CONFLICT
-- DO NOTHING) and deleted rows, including FK cascades. Account updates are
-- ordered to keep concurrent multi-account batches from taking opposite locks.
ALTER TABLE accounts ADD COLUMN events_total BIGINT NOT NULL DEFAULT 0;
UPDATE accounts a SET events_total = counts.total
FROM (SELECT account_id, count(*) AS total FROM events GROUP BY account_id) counts
WHERE counts.account_id = a.account_id;

CREATE FUNCTION count_inserted_events() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE delta record;
BEGIN
    FOR delta IN SELECT account_id, count(*) AS total FROM inserted_events
                 GROUP BY account_id ORDER BY account_id LOOP
        UPDATE accounts SET events_total = events_total + delta.total
        WHERE account_id = delta.account_id;
    END LOOP;
    RETURN NULL;
END;
$$;
CREATE FUNCTION count_deleted_events() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE delta record;
BEGIN
    FOR delta IN SELECT account_id, count(*) AS total FROM deleted_events
                 GROUP BY account_id ORDER BY account_id LOOP
        UPDATE accounts SET events_total = events_total - delta.total
        WHERE account_id = delta.account_id;
    END LOOP;
    RETURN NULL;
END;
$$;
CREATE TRIGGER events_count_insert AFTER INSERT ON events
REFERENCING NEW TABLE AS inserted_events
FOR EACH STATEMENT EXECUTE FUNCTION count_inserted_events();
CREATE TRIGGER events_count_delete AFTER DELETE ON events
REFERENCING OLD TABLE AS deleted_events
FOR EACH STATEMENT EXECUTE FUNCTION count_deleted_events();

-- Leave cleanup survives cancellation, contention, and process restarts.
CREATE TABLE room_purge_intents (
    account_id UUID NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    room_id TEXT NOT NULL,
    through_event_id BIGINT NOT NULL,
    PRIMARY KEY (account_id, room_id)
);

-- A delayed cleanup removes only the event generation captured at leave.
-- Rejoining or receiving later data preserves the current room metadata.
CREATE FUNCTION preserve_room_after_purge(account UUID, room TEXT, through_id BIGINT)
RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
    SELECT EXISTS (SELECT 1 FROM events
                   WHERE account_id = account AND room_id = room AND id > through_id)
        OR EXISTS (SELECT 1 FROM room_state rs JOIN accounts a USING (account_id)
                   WHERE rs.account_id = account AND rs.room_id = room
                     AND rs.event_type = 'm.room.member' AND rs.state_key = a.user_id
                     AND rs.content->>'membership' = 'join');
$$;
