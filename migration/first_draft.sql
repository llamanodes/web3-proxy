CREATE TABLE users (
  id SERIAL PRIMARY KEY,
  chain INT,
  primary_address VARCHAR(42),
  description VARCHAR(255),
  email VARCHAR(320),
)

-- TODO: foreign key
-- TODO: how should we store addresses?
-- TODO: creation time?
-- TODO: permissions. likely similar to infura
CREATE TABLE secondary_users (
  id SERIAL PRIMARY KEY,
  users_id BIGINT,
  secondary_address VARCHAR(42),
  chain INT,
  description VARCHAR,
  email VARCHAR(320),
)

-- TODO: creation time?
CREATE TABLE blocklist (
  id SERIAL PRIMARY KEY,
  blocked_address VARCHAR,
  chain INT,
  reason TEXT,
)

-- TODO: foreign key
-- TODO: index on api_key
-- TODO: what size for api_key
-- TODO: track active with a timestamp?
-- TODO: creation time?
-- TODO: requests_per_second INT,
-- TODO: requests_per_day INT,
-- TODO: more security features. likely similar to infura
CREATE TABLE user_keys (
  id SERIAL PRIMARY KEY,
  users_id BIGINT,
  api_key VARCHAR,
  description VARCHAR,
  private_txs BOOLEAN,
  active BOOLEAN,
)
