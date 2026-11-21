-- Quotes of route subscriptions that asked for the fees in the next steps of their outcome: how
-- their routing went, what that outcome needs, and the latest outcome asked for
CREATE TABLE quotes (
    id INTEGER PRIMARY KEY,
    -- Shared by the quotes found by one routing: the ID of its first quote
    routing_id INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    ip TEXT NOT NULL,
    token_in TEXT NOT NULL,
    token_out TEXT NOT NULL,
    swap_type TEXT NOT NULL CHECK (swap_type IN ('exact_in', 'exact_out')),
    -- Requested amount in for exact_in, amount out for exact_out
    amount TEXT NOT NULL,
    referrer_id TEXT,
    trader_account_id TEXT NOT NULL,
    -- Of routing on every DEX
    routing_duration_ms INTEGER NOT NULL,
    dex_id TEXT NOT NULL,
    -- Of routing on the quote's DEX
    dex_duration_ms INTEGER NOT NULL,
    -- Amount out for exact_in, amount in for exact_out
    estimated_amount TEXT NOT NULL,
    fee_amount TEXT NOT NULL,
    -- JSON of the fee transactions' execution instructions
    fee_instructions TEXT NOT NULL,
    quote_improvement_receiver TEXT,
    -- The latest outcome of the quote's transactions, NULL until one is asked for
    outcome_at TEXT,
    -- JSON array of the transactions' hashes
    tx_hashes TEXT,
    -- Whether the swap went through
    charge_fee INTEGER,
    outcome_amount_in TEXT,
    outcome_amount_out TEXT,
    quote_improvement_fee TEXT,
    settled INTEGER
);

CREATE INDEX quotes_created_at ON quotes (created_at);
CREATE INDEX quotes_routing_id ON quotes (routing_id);

-- Routing is only recorded with the quotes it found
DROP TABLE query_routes;
DROP TABLE queries;
