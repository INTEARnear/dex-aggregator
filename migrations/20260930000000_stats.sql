CREATE TABLE queries (
    id INTEGER PRIMARY KEY,
    timestamp TEXT NOT NULL,
    ip TEXT NOT NULL,
    token_in TEXT NOT NULL,
    token_out TEXT NOT NULL,
    swap_type TEXT NOT NULL CHECK (swap_type IN ('exact_in', 'exact_out')),
    amount TEXT NOT NULL,
    referrer_id TEXT,
    trader_account_id TEXT,
    duration_ms INTEGER NOT NULL
);

CREATE INDEX queries_timestamp ON queries (timestamp);

CREATE TABLE query_routes (
    query_id INTEGER NOT NULL REFERENCES queries (id),
    dex_id TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('found', 'not_found', 'timed_out', 'panicked')),
    PRIMARY KEY (query_id, dex_id)
);
