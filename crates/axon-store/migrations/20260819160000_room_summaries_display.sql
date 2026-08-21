-- Fold list-driving room_state into room_summaries (ADR 0094 follow-up).
--
-- The first room_summaries migration removed the events-table aggregate, but
-- list_rooms still ran seven correlated room_state / room_unread_counts
-- subqueries plus two NOT EXISTS anti-joins per room. On a 3,601-room account
-- that was 29.7 s of nested-loop point lookups — the remaining TTFB after the
-- aggregate was gone. Display fields and leave/tombstone visibility are cheap
-- to maintain at write time and turn the read into a scan of one row per room.
-- Unread counts stay in room_unread_counts (ADR 0070) and are still joined.
--
-- Do not edit 20260819120000_room_summaries.sql: production has already
-- applied it.
ALTER TABLE room_summaries
    ADD COLUMN name TEXT,
    ADD COLUMN topic TEXT,
    ADD COLUMN avatar_url TEXT,
    ADD COLUMN canonical_alias TEXT,
    ADD COLUMN room_type TEXT,
    ADD COLUMN hidden_left BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN hidden_tombstoned BOOLEAN NOT NULL DEFAULT FALSE;

-- Snapshot current state onto every existing summary. Missing tuples stay
-- NULL / false, matching the old "no row means show the room / no name".
-- The target table is joined via a subquery: PostgreSQL forbids referencing
-- the UPDATE target from FROM-clause JOIN ON conditions.
UPDATE room_summaries s
SET name = src.name,
    topic = src.topic,
    avatar_url = src.avatar_url,
    canonical_alias = src.canonical_alias,
    room_type = src.room_type,
    hidden_left = src.hidden_left,
    hidden_tombstoned = src.hidden_tombstoned
FROM (
    SELECT
        s2.account_id,
        s2.room_id,
        name_rs.content->>'name' AS name,
        topic_rs.content->>'topic' AS topic,
        avatar_rs.content->>'url' AS avatar_url,
        alias_rs.content->>'alias' AS canonical_alias,
        create_rs.content->>'type' AS room_type,
        COALESCE(mem_rs.content->>'membership' IN ('leave', 'ban'), FALSE)
            AS hidden_left,
        tomb_rs.event_id IS NOT NULL AS hidden_tombstoned
    FROM room_summaries s2
    JOIN accounts ac ON ac.account_id = s2.account_id
    LEFT JOIN room_state name_rs
        ON name_rs.account_id = s2.account_id AND name_rs.room_id = s2.room_id
       AND name_rs.event_type = 'm.room.name' AND name_rs.state_key = ''
    LEFT JOIN room_state topic_rs
        ON topic_rs.account_id = s2.account_id AND topic_rs.room_id = s2.room_id
       AND topic_rs.event_type = 'm.room.topic' AND topic_rs.state_key = ''
    LEFT JOIN room_state avatar_rs
        ON avatar_rs.account_id = s2.account_id AND avatar_rs.room_id = s2.room_id
       AND avatar_rs.event_type = 'm.room.avatar' AND avatar_rs.state_key = ''
    LEFT JOIN room_state alias_rs
        ON alias_rs.account_id = s2.account_id AND alias_rs.room_id = s2.room_id
       AND alias_rs.event_type = 'm.room.canonical_alias' AND alias_rs.state_key = ''
    LEFT JOIN room_state create_rs
        ON create_rs.account_id = s2.account_id AND create_rs.room_id = s2.room_id
       AND create_rs.event_type = 'm.room.create' AND create_rs.state_key = ''
    LEFT JOIN room_state mem_rs
        ON mem_rs.account_id = s2.account_id AND mem_rs.room_id = s2.room_id
       AND mem_rs.event_type = 'm.room.member' AND mem_rs.state_key = ac.user_id
    LEFT JOIN room_state tomb_rs
        ON tomb_rs.account_id = s2.account_id AND tomb_rs.room_id = s2.room_id
       AND tomb_rs.event_type = 'm.room.tombstone' AND tomb_rs.state_key = ''
) src
WHERE s.account_id = src.account_id AND s.room_id = src.room_id;

-- Copy current room_state onto a newly inserted summary (first event in a
-- room). State often lands before the first timeline event; without this the
-- INSERT would leave name/hidden NULL until the next state write.
CREATE FUNCTION refresh_room_summary_display(p_account_id UUID, p_room_id TEXT)
RETURNS VOID
LANGUAGE plpgsql
AS $$
DECLARE
    v_user_id TEXT;
BEGIN
    SELECT user_id INTO v_user_id
    FROM accounts
    WHERE account_id = p_account_id;

    UPDATE room_summaries s
    SET name = (
            SELECT rs.content->>'name' FROM room_state rs
            WHERE rs.account_id = p_account_id AND rs.room_id = p_room_id
              AND rs.event_type = 'm.room.name' AND rs.state_key = ''
        ),
        topic = (
            SELECT rs.content->>'topic' FROM room_state rs
            WHERE rs.account_id = p_account_id AND rs.room_id = p_room_id
              AND rs.event_type = 'm.room.topic' AND rs.state_key = ''
        ),
        avatar_url = (
            SELECT rs.content->>'url' FROM room_state rs
            WHERE rs.account_id = p_account_id AND rs.room_id = p_room_id
              AND rs.event_type = 'm.room.avatar' AND rs.state_key = ''
        ),
        canonical_alias = (
            SELECT rs.content->>'alias' FROM room_state rs
            WHERE rs.account_id = p_account_id AND rs.room_id = p_room_id
              AND rs.event_type = 'm.room.canonical_alias' AND rs.state_key = ''
        ),
        room_type = (
            SELECT rs.content->>'type' FROM room_state rs
            WHERE rs.account_id = p_account_id AND rs.room_id = p_room_id
              AND rs.event_type = 'm.room.create' AND rs.state_key = ''
        ),
        hidden_left = COALESCE((
            SELECT rs.content->>'membership' IN ('leave', 'ban')
            FROM room_state rs
            WHERE rs.account_id = p_account_id AND rs.room_id = p_room_id
              AND rs.event_type = 'm.room.member' AND rs.state_key = v_user_id
        ), FALSE),
        hidden_tombstoned = EXISTS (
            SELECT 1 FROM room_state rs
            WHERE rs.account_id = p_account_id AND rs.room_id = p_room_id
              AND rs.event_type = 'm.room.tombstone' AND rs.state_key = ''
        )
    WHERE s.account_id = p_account_id AND s.room_id = p_room_id;
END;
$$;
