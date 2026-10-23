-- Account-scoped preview authorization searches current image-pack state.
-- No sender-controlled JSON or MXC values are stored in this B-tree index.
CREATE INDEX room_state_image_pack_lookup ON room_state (account_id, room_id)
    WHERE event_type IN ('m.room.image_pack', 'im.ponies.room_emotes');
