-- Whether an APNs device wants the unread total as its icon badge (ADR 0129).
--
-- False for every existing row, so a client built before ADR 0129 keeps the
-- ADR 0113 payloads until it registers again and opts in. The client sends its
-- app-icon badge setting here. FCM rows store the value and nothing reads it.

ALTER TABLE push_registrations
    ADD COLUMN badge BOOLEAN NOT NULL DEFAULT false;
