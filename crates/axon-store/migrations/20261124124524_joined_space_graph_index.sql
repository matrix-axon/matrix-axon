-- Page account-wide space relationships by the stable state-tuple key
-- without scanning membership and other unrelated state.
CREATE INDEX room_state_space_graph
    ON room_state (account_id, room_id, event_type, state_key)
    WHERE event_type IN ('m.space.child', 'm.space.parent');
