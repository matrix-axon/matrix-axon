-- Preserve the first count migration for databases already upgraded.
-- Unknown counts can retain a positive invalidation watermark independently.
ALTER TABLE room_summaries DROP CONSTRAINT room_member_counts_complete;
ALTER TABLE room_summaries ADD CONSTRAINT room_member_counts_complete CHECK (
    (joined_member_count IS NULL AND invited_member_count IS NULL)
    OR (joined_member_count IS NOT NULL AND invited_member_count IS NOT NULL AND member_counts_observed_at IS NOT NULL)
);

CREATE OR REPLACE FUNCTION clear_left_room_member_counts() RETURNS TRIGGER AS $$
BEGIN
    NEW.joined_member_count := NULL;
    NEW.invited_member_count := NULL;
    NEW.member_counts_observed_at := GREATEST(
        OLD.member_counts_observed_at,
        NEW.member_counts_observed_at,
        floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Ordinary joined-room activity never invokes this function. Record the
-- leave transition even with no counts yet, fencing delayed observations.
DROP TRIGGER room_summaries_clear_left_member_counts ON room_summaries;
CREATE TRIGGER room_summaries_clear_left_member_counts
    BEFORE UPDATE ON room_summaries
    FOR EACH ROW
    WHEN (NEW.hidden_left AND (NOT OLD.hidden_left OR NEW.joined_member_count IS NOT NULL))
    EXECUTE FUNCTION clear_left_room_member_counts();
