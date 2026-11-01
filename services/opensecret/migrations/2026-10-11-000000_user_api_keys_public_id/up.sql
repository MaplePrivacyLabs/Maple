-- A random, non-secret identifier for each API key. It is the only key
-- identifier that may leave the enclave (for example in usage events) and is
-- unrelated to the key secret and its hash. Existing keys each get their own
-- value because the default is evaluated per row.
ALTER TABLE user_api_keys
    ADD COLUMN public_id UUID NOT NULL DEFAULT gen_random_uuid() UNIQUE;
