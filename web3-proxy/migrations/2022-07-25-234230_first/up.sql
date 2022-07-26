CREATE TABLE users (
  id SERIAL PRIMARY KEY,
  primary_address VARCHAR NOT NULL,
  chain INT NOT NULL,
  description VARCHAR,
  email VARCHAR DEFAULT NULL,
)

CREATE TABLE secondary_users (
  id SERIAL PRIMARY KEY,
  -- TODO: foreign key
  users_id BIGINT,
  -- TODO: how should we store addresses?
  secondary_address VARCHAR NOT NULL,
  chain INT NOT NULL,
  description VARCHAR,
  -- TODO: creation time?
  -- TODO: permissions. likely similar to infura
)

CREATE TABLE blocklist (
  id SERIAL PRIMARY KEY,
  -- TODO: creation time?
  blocked_address VARCHAR NOT NULL,
  chain INT NOT NULL,
  reason TEXT,
)

CREATE TABLE user_keys (
  id SERIAL PRIMARY KEY,
  -- TODO: foreign key
  users_id BIGINT,
  -- TODO: index on api_key
  api_key VARCHAR NOT NULL,
  description VARCHAR,
  private_txs BOOLEAN,
  -- TODO: track active with a timestamp?
  active BOOLEAN,
  -- TODO: creation time?
  -- TODO: requests_per_second INT,
  -- TODO: requests_per_day INT,
  -- TODO: more security features. likely similar to infura
)
