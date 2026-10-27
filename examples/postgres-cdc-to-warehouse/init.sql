CREATE TABLE orders (
  id bigint PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
  customer text NOT NULL,
  amount_cents bigint NOT NULL,
  status text NOT NULL DEFAULT 'new'
);
CREATE PUBLICATION orders_pub FOR TABLE orders;
INSERT INTO orders (customer, amount_cents) VALUES ('ada', 1250), ('grace', 9900), ('linus', 300);
