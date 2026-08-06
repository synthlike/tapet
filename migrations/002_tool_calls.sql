CREATE TABLE tool_calls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    room_call_id INTEGER NOT NULL REFERENCES room_calls(id) ON DELETE CASCADE,
    provider_call_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    arguments TEXT NOT NULL,
    status TEXT NOT NULL CHECK (
        status IN ('proposed', 'approved', 'denied', 'running', 'completed', 'failed')
    ),
    result_bytes INTEGER,
    result_lines INTEGER,
    error TEXT,
    proposed_at INTEGER NOT NULL,
    decided_at INTEGER,
    started_at INTEGER,
    finished_at INTEGER,
    UNIQUE (room_call_id, provider_call_id)
);

CREATE INDEX tool_calls_by_room_call
    ON tool_calls(room_call_id, id);
