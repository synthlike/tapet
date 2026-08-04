CREATE TABLE rooms (
    id TEXT PRIMARY KEY,
    description TEXT NOT NULL,
    system_prompt TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE room_participants (
    room_id TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    agent_name TEXT NOT NULL,
    provider_kind TEXT NOT NULL CHECK (provider_kind = 'openai'),
    base_url TEXT NOT NULL,
    api_key_env TEXT NOT NULL,
    model TEXT NOT NULL,
    system_prompt TEXT NOT NULL,
    position INTEGER NOT NULL,
    PRIMARY KEY (room_id, agent_name),
    UNIQUE (room_id, position)
);

CREATE TABLE room_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    speaker_kind TEXT NOT NULL CHECK (speaker_kind IN ('user', 'agent')),
    speaker_name TEXT,
    content TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    CHECK (
        (speaker_kind = 'user' AND speaker_name IS NULL) OR
        (speaker_kind = 'agent' AND speaker_name IS NOT NULL)
    )
);

CREATE INDEX room_messages_by_room
    ON room_messages(room_id, id);

CREATE TABLE room_calls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    participant_name TEXT NOT NULL,
    user_message_id INTEGER NOT NULL REFERENCES room_messages(id),
    assistant_message_id INTEGER REFERENCES room_messages(id),
    status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed')),
    provider_response_id TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    error TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    FOREIGN KEY (room_id, participant_name)
        REFERENCES room_participants(room_id, agent_name)
);

CREATE INDEX room_calls_by_room
    ON room_calls(room_id, id);
