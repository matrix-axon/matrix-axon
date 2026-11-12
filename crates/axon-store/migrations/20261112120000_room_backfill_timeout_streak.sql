-- A backfill room's timeout streak. See ADR 0122's amendment of 2026-11-12
-- ("one slow room does not shut the gate", #1037).
--
-- `timeout_streak` counts the room's timed-out pages since its last fast page.
-- A timed-out page's retry delay grows with it, so a room that alternates slow
-- successes with timeouts keeps backing off further. Unlike
-- `consecutive_failures`, a page that succeeds slowly does not reset it; only a
-- fast page (under half the page timeout) does.

ALTER TABLE room_backfill
    ADD COLUMN timeout_streak INTEGER NOT NULL DEFAULT 0;
