-- Incremental room-list reads (ADR 0134).
--
-- `list_changed_at` is when a value `GET /v1/rooms` shows for this room last
-- changed, as far as the summary row and `m.direct` can tell. A delta read
-- returns the rows whose `list_changed_at`, unread-count `updated_at`, or
-- `m.tag` `updated_at` is after its cursor, less an overlap.
--
-- It is not `updated_at`: every room-state write rewrites the display columns
-- through `refresh_room_summary_display` whether or not they changed, and
-- member counts and membership epochs are not in the list, so `updated_at`
-- moves on nearly every row after a restart.
--
-- Existing rows share the migration's time; no client holds a cursor yet.
-- New rows take the statement clock, not the transaction start, so a stamp is
-- the time of the write itself (ADR 0134, "The cursor overlaps the write
-- window").
ALTER TABLE room_summaries ADD COLUMN list_changed_at TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE room_summaries ALTER COLUMN list_changed_at SET DEFAULT clock_timestamp();

-- The column list must match the `room_summaries` columns `RoomDto` shows
-- (`Store::list_rooms`). A listed column left out here never reaches a delta
-- when only it changes. An UPDATE that sets `list_changed_at` itself and no
-- listed column, as the triggers below do, keeps its own value.
CREATE FUNCTION stamp_room_list_change() RETURNS TRIGGER AS $$
BEGIN
    NEW.list_changed_at := clock_timestamp();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER room_summaries_stamp_list_change
    BEFORE UPDATE ON room_summaries
    FOR EACH ROW
    WHEN ((NEW.last_activity_ts, NEW.last_event_id, NEW.name, NEW.topic,
           NEW.avatar_url, NEW.canonical_alias, NEW.room_type, NEW.display_name,
           NEW.hidden_left, NEW.hidden_tombstoned)
          IS DISTINCT FROM
          (OLD.last_activity_ts, OLD.last_event_id, OLD.name, OLD.topic,
           OLD.avatar_url, OLD.canonical_alias, OLD.room_type, OLD.display_name,
           OLD.hidden_left, OLD.hidden_tombstoned))
    EXECUTE FUNCTION stamp_room_list_change();

-- A deleted unread-count row leaves nothing to carry a time, and the list then
-- shows the room's counts as 0. Inserted and updated rows carry their own
-- `updated_at`, which the delta reads directly, so only deletes write here.
CREATE FUNCTION stamp_room_list_unread_delete() RETURNS TRIGGER AS $$
BEGIN
    UPDATE room_summaries s
    SET list_changed_at = clock_timestamp()
    FROM gone g
    WHERE s.account_id = g.account_id AND s.room_id = g.room_id;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER room_unread_counts_stamp_list_change
    AFTER DELETE ON room_unread_counts
    REFERENCING OLD TABLE AS gone
    FOR EACH STATEMENT
    EXECUTE FUNCTION stamp_room_list_unread_delete();

-- The room ids in an `m.direct` content object: every string in every array
-- value of the top-level object, as `room_ids_in_direct_map` reads it in Rust.
-- Anything else (a non-object, a non-array value, a non-string element) is
-- ignored rather than an error, so a malformed map never fails its write.
CREATE FUNCTION direct_map_room_ids(content JSONB) RETURNS SETOF TEXT AS $$
    SELECT e.value #>> '{}'
    FROM jsonb_each(CASE WHEN jsonb_typeof(content) = 'object'
                         THEN content ELSE '{}'::jsonb END) AS m(key, value)
    CROSS JOIN LATERAL jsonb_array_elements(
        CASE WHEN jsonb_typeof(m.value) = 'array' THEN m.value ELSE '[]'::jsonb END
    ) AS e(value)
    WHERE jsonb_typeof(e.value) = 'string'
$$ LANGUAGE sql IMMUTABLE;

-- `is_direct` is derived from the account's one global `m.direct` value, so a
-- write to it can flip many rooms at once. Bump exactly the rooms whose
-- membership in the map changed: those in the old map or the new one but not
-- both. A rewrite with the same room ids bumps nothing.
CREATE FUNCTION stamp_room_list_direct_change() RETURNS TRIGGER AS $$
DECLARE
    v_account_id UUID;
    v_old JSONB;
    v_new JSONB;
BEGIN
    IF TG_OP = 'INSERT' THEN
        v_account_id := NEW.account_id;
        v_new := NEW.content;
    ELSIF TG_OP = 'DELETE' THEN
        v_account_id := OLD.account_id;
        v_old := OLD.content;
    ELSE
        v_account_id := NEW.account_id;
        v_old := OLD.content;
        v_new := NEW.content;
    END IF;
    UPDATE room_summaries s
    SET list_changed_at = clock_timestamp()
    WHERE s.account_id = v_account_id
      AND s.room_id IN (
          (SELECT direct_map_room_ids(v_new) EXCEPT SELECT direct_map_room_ids(v_old))
          UNION
          (SELECT direct_map_room_ids(v_old) EXCEPT SELECT direct_map_room_ids(v_new))
      );
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER account_data_stamp_direct_insert
    AFTER INSERT ON account_data
    FOR EACH ROW
    WHEN (NEW.event_type = 'm.direct' AND NEW.room_id = '')
    EXECUTE FUNCTION stamp_room_list_direct_change();
CREATE TRIGGER account_data_stamp_direct_update
    AFTER UPDATE ON account_data
    FOR EACH ROW
    WHEN (NEW.event_type = 'm.direct' AND NEW.room_id = ''
          AND NEW.content IS DISTINCT FROM OLD.content)
    EXECUTE FUNCTION stamp_room_list_direct_change();
CREATE TRIGGER account_data_stamp_direct_delete
    AFTER DELETE ON account_data
    FOR EACH ROW
    WHEN (OLD.event_type = 'm.direct' AND OLD.room_id = '')
    EXECUTE FUNCTION stamp_room_list_direct_change();

-- An account's rooms are listed only while it is `active`. Entering or leaving
-- that state touches none of the rows above, so the account carries its own
-- stamp, and a delta returns every row of an account stamped after its cursor:
-- as rooms when the account became active, as removed rooms when it stopped.
-- The listed count alone cannot stand in for this, since one account's rooms
-- returning can cancel another's leaving (ADR 0134, "Removals").
--
-- Existing accounts never changed state as far as any cursor knows. Only a
-- change into or out of `active` stamps; moves between other states list
-- nothing either way.
ALTER TABLE accounts ADD COLUMN list_changed_at TIMESTAMPTZ NOT NULL DEFAULT '-infinity';
CREATE FUNCTION stamp_account_list_change() RETURNS TRIGGER AS $$
BEGIN
    NEW.list_changed_at := clock_timestamp();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER accounts_stamp_list_change
    BEFORE UPDATE OF state ON accounts
    FOR EACH ROW
    WHEN ((OLD.state = 'active') IS DISTINCT FROM (NEW.state = 'active'))
    EXECUTE FUNCTION stamp_account_list_change();
