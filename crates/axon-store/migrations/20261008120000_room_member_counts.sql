-- SDK summary counts, independent of lazily loaded membership state.
-- Existing rows remain unknown until the bounded watcher observes the SDK.
ALTER TABLE room_summaries
    ADD COLUMN joined_member_count BIGINT CHECK (joined_member_count > 0),
    ADD COLUMN invited_member_count BIGINT CHECK (invited_member_count >= 0),
    ADD COLUMN member_counts_observed_at BIGINT,
    ADD CONSTRAINT room_member_counts_complete CHECK (
        (joined_member_count IS NULL AND invited_member_count IS NULL AND member_counts_observed_at IS NULL)
        OR (joined_member_count IS NOT NULL AND invited_member_count IS NOT NULL AND member_counts_observed_at IS NOT NULL)
    );

-- Local membership writes and background observations can race. A left-room
-- projection atomically invalidates counts and blocks late joined observations.
CREATE FUNCTION clear_left_room_member_counts() RETURNS TRIGGER AS $$
BEGIN
    IF NEW.hidden_left THEN
        NEW.joined_member_count := NULL;
        NEW.invited_member_count := NULL;
        NEW.member_counts_observed_at := NULL;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER room_summaries_clear_left_member_counts
    BEFORE UPDATE ON room_summaries
    FOR EACH ROW EXECUTE FUNCTION clear_left_room_member_counts();
