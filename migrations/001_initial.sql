CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    agent_name TEXT NOT NULL,
    provider_kind TEXT NOT NULL CHECK (provider_kind = 'openai'),
    base_url TEXT NOT NULL,
    api_key_env TEXT NOT NULL,
    model TEXT NOT NULL,
    system_prompt TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    content TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX messages_by_session
    ON messages(session_id, id);

CREATE TABLE model_calls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    user_message_id INTEGER NOT NULL REFERENCES messages(id),
    assistant_message_id INTEGER REFERENCES messages(id),
    status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed')),
    provider_response_id TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    error TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE INDEX model_calls_by_session
    ON model_calls(session_id, id);
