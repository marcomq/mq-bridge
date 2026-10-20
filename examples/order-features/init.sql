CREATE TABLE customers (
  id   bigint PRIMARY KEY,
  name text NOT NULL,
  tier text NOT NULL
);
INSERT INTO customers VALUES (1, 'Ada', 'gold'), (2, 'Grace', 'standard');

-- Orders with what was known about the customer before each one.
CREATE TABLE orders_enriched (
  id          bigint PRIMARY KEY,
  customer_id bigint NOT NULL,
  amount      numeric(12, 2) NOT NULL,
  created_at  timestamptz NOT NULL,
  customer    jsonb,
  history     jsonb
);
-- Orders far above the customer's usual amount, for a person to check.
CREATE TABLE orders_review (LIKE orders_enriched INCLUDING ALL);
