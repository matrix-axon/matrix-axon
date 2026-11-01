-- The pre-proof timeline treated delivered same-room redactions as applied.
-- Preserve that privacy boundary for every event type before accepting traffic,
-- including moderator redactions whose historical power levels are unavailable.
-- Newly ingested redactions continue to require captured or immutable authority.
WITH proven AS (
INSERT INTO event_redaction_proofs
    (account_id, room_id, redaction_event_id, target_event_id)
SELECT redaction.account_id, redaction.room_id, redaction.event_id, target.event_id
FROM events redaction
JOIN events target ON target.account_id = redaction.account_id
    AND target.room_id = redaction.room_id AND target.event_id = redaction.redacts
WHERE redaction.event_type = 'm.room.redaction'
ON CONFLICT DO NOTHING
RETURNING account_id, target_event_id
), affected AS (
    SELECT target.account_id, target.event_id, target.relates_to
    FROM proven JOIN events target ON target.account_id = proven.account_id
        AND target.event_id = proven.target_event_id
)
INSERT INTO search_outbox (account_id, event_id)
SELECT account_id, event_id FROM affected
UNION
SELECT account_id, relates_to->>'event_id' FROM affected
WHERE relates_to->>'event_id' IS NOT NULL;
