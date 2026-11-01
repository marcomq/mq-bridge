-- 1M users keyed by the request path the load generator draws: /u/000000 .. /u/999999.
CREATE TABLE users (
    path text PRIMARY KEY,
    id   bigint NOT NULL,
    name text NOT NULL,
    tier text NOT NULL
);

INSERT INTO users
SELECT '/u/' || lpad(g::text, 6, '0'), g, 'user_' || g, (ARRAY['free', 'pro', 'team'])[1 + g % 3]
FROM generate_series(0, 999999) AS g;

ANALYZE users;
