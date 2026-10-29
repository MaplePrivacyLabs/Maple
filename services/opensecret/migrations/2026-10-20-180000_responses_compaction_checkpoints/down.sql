-- Derived versioned checkpoints can be discarded; canonical transcript rows are untouched.
DELETE FROM conversation_summaries WHERE format_version = 1;
DROP INDEX conversation_summaries_checkpoint_response;
DROP INDEX conversation_summaries_checkpoint_generation;
ALTER TABLE conversation_summaries
    DROP CONSTRAINT valid_compaction_checkpoint,
    DROP COLUMN after_item_uuid,
    DROP COLUMN origin_response_uuid,
    DROP COLUMN source_fingerprint,
    DROP COLUMN generation,
    DROP COLUMN format_version;
