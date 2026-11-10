-- Deterministic /stats fixture, shared by tests/stats.rs and the Node oracle
-- (bench/stats-oracle.mjs) that generates the expected body.
--
-- Every timestamp is either far in the past (<= now-30d forever, so the
-- 7d/30d added_at and price snapshots always include it) or far in the
-- future (> now-7d forever, so the change windows always exclude it). No
-- relative-to-"now" data: the expected body stays valid across test runs.
INSERT INTO users (id, username, password_hash, role, share_token, api_key) VALUES
  (1, 'stats_rich', 'x', 'member', 'share-stats-1', 'tok-api-stats'),
  (2, 'stats_empty', 'x', 'member', 'share-stats-2', NULL),
  (3, 'stats_other', 'x', 'member', 'share-stats-3', NULL);
INSERT INTO sessions (token, user_id, expires_at) VALUES
  ('tok-stats-rich', 1, '2099-01-01 00:00:00'),
  ('tok-stats-empty', 2, '2099-01-01 00:00:00'),
  ('tok-stats-other', 3, '2099-01-01 00:00:00');

-- sets: printed_total wins, total is the fallback, both 0/NULL => size NULL.
INSERT INTO sets (id, name, printed_total, total) VALUES
  ('lea', 'Limited Edition Alpha', 364, NULL),
  ('2ed', 'Unlimited Edition', 302, 350),
  ('m11', 'Magic 2011', NULL, 397),
  ('xyz', 'Mystery Set', 0, 0);

INSERT INTO card_cache (id, name, set_id, set_name, rarity, supertype, types, subtypes, cmc, color_identity, price_trend, price_normal, price_holofoil, price_estimated) VALUES
  ('c-sol',     'Sol Ring',       'lea', 'Limited Edition Alpha', 'Uncommon', 'Artifact', '["Artifact"]', '[]', 1, '[]', 2.5, 2.2, NULL, 0),
  ('c-falcon',  'Zephyr Falcon',  'lea', 'Limited Edition Alpha', 'Rare',     'Creature', '["Creature"]', '[]', 3, '["U"]', 10.25, 9.99, 12.34, 0),
  ('c-beta',    'Basalt Monitor', '2ed', 'Unlimited Edition',     'Rare',     'Artifact', '[]', '[]', 2, '[]', 5.55, 3.45, 5.0, 1),
  ('c-plains',  'Plains',         'm11', 'Magic 2011',            'Common',   'Land',     '[]', '["Land"]', 0, '[]', 0.1, NULL, NULL, 0),
  ('c-proxy',   'Mirror Golem',   'xyz', 'Mystery Set',           'Rare',     'Artifact', '["Artifact","Creature"]', '[]', 4, '[]', 4.0, NULL, NULL, 0),
  ('c-free',    'Null Price',     'lea', NULL,                    NULL,       NULL,       '[]', '[]', 0, '[]', NULL, NULL, NULL, 0);

-- user 1 entries. added_at: e2 is future-dated (change-window flip), the
-- rest are ancient (inside both windows). is_proxy on e5 exercises the
-- Proxy rarity + zeroed price + change-window exclusion.
INSERT INTO collection (id, card_id, quantity, condition, printing, purchase_price, added_at, is_trade, user_id, is_proxy) VALUES
  (1, 'c-sol',    3, 'Near Mint', 'Holofoil', 1.0,  '2020-01-01 12:00:00', 0, 1, 0),
  (2, 'c-falcon', 2, 'Near Mint', 'Holofoil', 4.0,  '2099-01-01 00:00:00', 0, 1, 0),
  (3, 'c-beta',   1, 'Lightly Played', 'Normal',   2.0,  '2020-01-02 00:00:00', 0, 1, 0),
  (4, 'c-plains', 5, 'Near Mint', 'Normal',   0.0,  '2020-01-03 00:00:00', 0, 1, 0),
  (5, 'c-proxy',  2, 'Moderately Played', 'Normal',   1.5,  '2020-01-04 00:00:00', 0, 1, 1),
  (6, 'c-free',   4, NULL,        'Normal',   NULL, '2020-01-05 00:00:00', 0, 1, 0),
  (7, 'c-sol',    100, 'Near Mint', 'Normal', 1.0,  '2020-01-01 00:00:00', 0, 3, 0);

-- Price history. The 2099 point keeps `current_history` above the
-- <= now-7d/-30d points forever, so change7d/change30d flip to
-- available:true with non-zero abs/pct. c-beta has no rows (history7 IS
-- NULL => excluded from the change sums) and c-proxy is proxy-only (its
-- row must be ignored through change_owned's is_proxy filter).
INSERT INTO price_history (card_id, price, recorded_at) VALUES
  ('c-sol',    2.0,  '2019-01-01 00:00:00'),
  ('c-sol',    2.2,  '2024-06-01 00:00:00'),
  ('c-sol',    2.5,  '2026-01-01 00:00:00'),
  ('c-sol',    3.0,  '2099-01-01 00:00:00'),
  ('c-falcon', 8.0,  '2019-01-01 00:00:00'),
  ('c-falcon', 11.0, '2026-01-01 00:00:00'),
  ('c-proxy',  4.0,  '2019-01-01 00:00:00');

-- Decks: built (checked_out 1), need-cards (missing falcon copies), and a
-- not-built deck whose only need is basic lands (exempt => not counted).
-- user 3's deck must never leak into user 1's ownership map.
INSERT INTO decks (id, user_id, name, checked_out) VALUES
  (10, 1, 'Built Deck', 1),
  (11, 1, 'Wish Deck', 0),
  (12, 1, 'Land Deck', NULL),
  (13, 3, 'Other Deck', 1);
INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES
  (10, 'c-falcon', 1),
  (11, 'c-falcon', 4),
  (12, 'c-plains', 20),
  (13, 'c-falcon', 4);
