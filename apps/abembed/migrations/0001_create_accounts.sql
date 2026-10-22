CREATE TABLE accounts (
    id INTEGER PRIMARY KEY,
    platform TEXT NOT NULL,
    account_id TEXT NOT NULL,
    label TEXT,
    region TEXT NOT NULL CHECK (region IN ('a', 'b', 'c', 'd')),
    credential TEXT NOT NULL,
    nonce TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 0,
    STATUS TEXT NOT NULL CHECK (STATUS IN ('active', 'needs_reauth', 'disabled')),
    status_reason TEXT,
    last_used_at INTEGER,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (platform, account_id)
);
