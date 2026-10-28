-- Append-only change log, as a warehouse table would hold it.
CREATE TABLE orders_changes (
  id bigint NOT NULL,
  customer text,
  amount_eur numeric,
  status text,
  _op text NOT NULL,
  _lsn text,
  _ord text,
  _synced_at timestamptz NOT NULL,
  CONSTRAINT plausible_amount CHECK (amount_eur IS NULL OR amount_eur < 1000)
);
