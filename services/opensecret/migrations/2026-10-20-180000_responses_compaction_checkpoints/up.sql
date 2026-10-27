-- Versioned user-key ciphertext replaces the unused legacy summary projection.
-- Coverage identities remain inside the encrypted envelope; only its digest is indexed.
ALTER TABLE conversation_summaries
    ADD COLUMN format_version INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN generation BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN source_fingerprint BYTEA,
    -- The first checkpoint can precede response persistence; provenance is not a FK.
    ADD COLUMN origin_response_uuid UUID,
    ADD COLUMN after_item_uuid UUID,
    ADD CONSTRAINT valid_compaction_checkpoint CHECK (
        format_version = 0 OR (
            format_version = 1 AND generation > 0 AND content_tokens >= 0
            AND message_count > 0 AND source_fingerprint IS NOT NULL
            AND octet_length(source_fingerprint) = 32 AND origin_response_uuid IS NOT NULL
        )
    );
CREATE UNIQUE INDEX conversation_summaries_checkpoint_generation
    ON conversation_summaries(conversation_id, generation) WHERE format_version = 1;
CREATE INDEX conversation_summaries_checkpoint_response
    ON conversation_summaries(user_id, conversation_id, origin_response_uuid, generation)
    WHERE format_version = 1;
