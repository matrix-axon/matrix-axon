-- Keep SDK-observed redaction evidence separate from retained content.
-- NULL preserves uncertainty for legacy rows and callers without evidence.
-- Later timeline redactions are read from the existing indexed events log;
-- they need no backfill, sweep, or cross-handler update ordering.
ALTER TABLE room_state ADD COLUMN redacted BOOLEAN;
ALTER TABLE room_state ADD COLUMN redaction_event_id TEXT;
ALTER TABLE room_state ADD CONSTRAINT room_state_redaction_evidence
    CHECK (redaction_event_id IS NULL OR redacted IS TRUE) NOT VALID;
-- Enforce new writes without scanning all legacy member-state rows on startup.
-- Old rows have NULL in both new columns and need no data backfill.
