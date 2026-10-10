-- Estimated amount of the route: amount out for exact_in queries, amount in for exact_out.
-- NULL when the route wasn't found.
ALTER TABLE query_routes ADD COLUMN estimated_amount TEXT;
