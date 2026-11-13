-- Why a stored event could not be decrypted (ADR 0136 decision 2).
--
-- Set only while the row is a UTD (content IS NULL), from matrix-sdk's
-- UnableToDecryptReason at ingest and on every re-decryption attempt, and
-- cleared when the row is back-filled. Rows stored before this migration keep
-- NULL until their next attempt.
--
-- utd_reason_detail is the withheld code for 'withheld' (e.g. m.unverified)
-- and the SDK verification level for 'sender_not_trusted'; NULL otherwise.
ALTER TABLE events
    ADD COLUMN utd_reason TEXT,
    ADD COLUMN utd_reason_detail TEXT;

-- Enforce new writes without scanning existing rows on startup: every existing
-- row has NULL in both columns.
ALTER TABLE events ADD CONSTRAINT events_utd_reason_kind
    CHECK (utd_reason IS NULL
        OR utd_reason IN ('missing_key', 'withheld', 'sender_not_trusted', 'other')) NOT VALID;
ALTER TABLE events ADD CONSTRAINT events_utd_reason_detail_needs_reason
    CHECK (utd_reason_detail IS NULL OR utd_reason IS NOT NULL) NOT VALID;
