-- The SDK's calculated room display name (ADR 0118). The value lives only in
-- the matrix-sdk state store, so existing rows stay NULL until the SDK
-- reconciler's first sweep writes them; nothing here can backfill it.
ALTER TABLE room_summaries
    ADD COLUMN display_name TEXT;
