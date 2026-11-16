-- Issue #59: a fifth sender_trust verdict, 'mismatched_sender'.
--
-- matrix-sdk-crypto reports VerificationLevel::MismatchedSender when the Megolm
-- session belongs to a different user than the event's sender. Every message
-- in an encrypted bridged room arrives this way (puppet sender, bridge bot's
-- session). M7c folded it into 'unverified'; it now keeps its own value so a
-- client can tell it from an unverified device.
--
-- Rows already stored keep 'unverified': the verdict is an immutable
-- first-write-wins snapshot, so no backfill. The check was declared inline
-- without a name, so it carries PostgreSQL's generated one. Both statements run
-- in the migration's single transaction.
ALTER TABLE event_sender_device_keys
    DROP CONSTRAINT event_sender_device_keys_sender_trust_check;

ALTER TABLE event_sender_device_keys
    ADD CONSTRAINT event_sender_device_keys_sender_trust_check
        CHECK (sender_trust IN (
            'verified', 'unverified', 'unknown', 'verification_violation',
            'mismatched_sender'
        ));
