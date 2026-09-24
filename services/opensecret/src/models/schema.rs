// @generated automatically by Diesel CLI.

pub mod sql_types {
    #[derive(diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "response_status"))]
    pub struct ResponseStatus;
}

diesel::table! {
    account_deletion_requests (id) {
        id -> Int4,
        user_id -> Uuid,
        project_id -> Int4,
        #[max_length = 255]
        hashed_secret -> Varchar,
        encrypted_code -> Bytea,
        expiration_time -> Timestamptz,
        created_at -> Timestamptz,
        completed_at -> Nullable<Timestamptz>,
        is_deleted -> Bool,
    }
}

diesel::table! {
    agent_schedule_runs (id) {
        id -> Int8,
        uuid -> Uuid,
        schedule_id -> Int8,
        user_id -> Uuid,
        agent_id -> Int8,
        scheduled_for -> Timestamptz,
        stale_after_at -> Timestamptz,
        status -> Text,
        attempt_count -> Int4,
        next_attempt_at -> Timestamptz,
        lease_owner -> Nullable<Text>,
        lease_expires_at -> Nullable<Timestamptz>,
        started_at -> Nullable<Timestamptz>,
        first_output_at -> Nullable<Timestamptz>,
        first_message_id -> Nullable<Uuid>,
        output_count -> Int4,
        notification_enqueued_at -> Nullable<Timestamptz>,
        completed_at -> Nullable<Timestamptz>,
        last_error -> Nullable<Text>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    agent_schedules (id) {
        id -> Int8,
        uuid -> Uuid,
        user_id -> Uuid,
        agent_id -> Int8,
        description -> Text,
        instruction_enc -> Bytea,
        schedule_kind -> Text,
        recurrence_type -> Nullable<Text>,
        schedule_spec -> Jsonb,
        timezone_mode -> Text,
        resolved_timezone -> Text,
        fixed_timezone -> Nullable<Text>,
        stale_after_minutes -> Int4,
        status -> Text,
        next_scheduled_for -> Nullable<Timestamptz>,
        last_scheduled_for -> Nullable<Timestamptz>,
        last_run_at -> Nullable<Timestamptz>,
        run_count -> Int4,
        cancelled_at -> Nullable<Timestamptz>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    agents (id) {
        id -> Int8,
        uuid -> Uuid,
        user_id -> Uuid,
        conversation_id -> Int8,
        kind -> Text,
        parent_agent_id -> Nullable<Int8>,
        display_name_enc -> Nullable<Bytea>,
        purpose_enc -> Nullable<Bytea>,
        created_by -> Text,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    app_data_migrations (name) {
        name -> Text,
        completed_at -> Timestamptz,
    }
}

diesel::table! {
    assistant_messages (id) {
        id -> Int8,
        uuid -> Uuid,
        conversation_id -> Int8,
        response_id -> Nullable<Int8>,
        user_id -> Uuid,
        content_enc -> Nullable<Bytea>,
        completion_tokens -> Int4,
        status -> Text,
        finish_reason -> Nullable<Text>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
        user_reaction -> Nullable<Text>,
    }
}

diesel::table! {
    conversation_projects (id) {
        id -> Int8,
        uuid -> Uuid,
        user_id -> Uuid,
        name_enc -> Bytea,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    conversation_summaries (id) {
        id -> Int8,
        uuid -> Uuid,
        user_id -> Uuid,
        conversation_id -> Int8,
        from_created_at -> Timestamptz,
        to_created_at -> Timestamptz,
        message_count -> Int4,
        content_enc -> Bytea,
        content_tokens -> Int4,
        embedding_enc -> Nullable<Bytea>,
        previous_summary_id -> Nullable<Int8>,
        created_at -> Timestamptz,
    }
}

diesel::table! {
    conversations (id) {
        id -> Int8,
        uuid -> Uuid,
        user_id -> Uuid,
        metadata_enc -> Nullable<Bytea>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
        project_id -> Nullable<Int8>,
        is_pinned -> Bool,
        last_activity_at -> Timestamptz,
    }
}

diesel::table! {
    email_opt_outs (user_id) {
        user_id -> Uuid,
        source -> Text,
        opted_out_at -> Timestamptz,
    }
}

diesel::table! {
    email_verifications (id) {
        id -> Int4,
        user_id -> Uuid,
        verification_code -> Uuid,
        is_verified -> Bool,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
        expires_at -> Timestamptz,
    }
}

diesel::table! {
    enclave_secrets (id) {
        id -> Int4,
        key -> Text,
        value -> Bytea,
    }
}

diesel::table! {
    invite_codes (id) {
        id -> Int4,
        code -> Uuid,
        org_id -> Int4,
        email -> Text,
        role -> Text,
        used -> Bool,
        expires_at -> Timestamptz,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    memory_blocks (id) {
        id -> Int8,
        uuid -> Uuid,
        user_id -> Uuid,
        label -> Text,
        value_enc -> Bytea,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    notification_deliveries (id) {
        id -> Int8,
        event_id -> Int8,
        push_device_id -> Int8,
        status -> Text,
        attempt_count -> Int4,
        next_attempt_at -> Timestamptz,
        lease_owner -> Nullable<Text>,
        lease_expires_at -> Nullable<Timestamptz>,
        provider_message_id -> Nullable<Text>,
        provider_status_code -> Nullable<Int4>,
        last_error -> Nullable<Text>,
        sent_at -> Nullable<Timestamptz>,
        invalidated_at -> Nullable<Timestamptz>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    notification_events (id) {
        id -> Int8,
        uuid -> Uuid,
        project_id -> Int4,
        user_id -> Uuid,
        kind -> Text,
        delivery_mode -> Text,
        priority -> Text,
        collapse_key -> Nullable<Text>,
        fallback_title -> Text,
        fallback_body -> Text,
        payload_enc -> Nullable<Bytea>,
        not_before_at -> Timestamptz,
        expires_at -> Nullable<Timestamptz>,
        created_at -> Timestamptz,
        cancelled_at -> Nullable<Timestamptz>,
    }
}

diesel::table! {
    oauth_providers (id) {
        id -> Int4,
        #[max_length = 255]
        name -> Varchar,
        auth_url -> Text,
        token_url -> Text,
        user_info_url -> Text,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    org_memberships (id) {
        id -> Int4,
        platform_user_id -> Uuid,
        org_id -> Int4,
        role -> Text,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    org_project_secrets (id) {
        id -> Int4,
        project_id -> Int4,
        key_name -> Text,
        secret_enc -> Bytea,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    org_projects (id) {
        id -> Int4,
        uuid -> Uuid,
        client_id -> Uuid,
        org_id -> Int4,
        name -> Text,
        description -> Nullable<Text>,
        status -> Text,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    orgs (id) {
        id -> Int4,
        uuid -> Uuid,
        name -> Text,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    password_reset_requests (id) {
        id -> Int4,
        user_id -> Uuid,
        #[max_length = 255]
        hashed_secret -> Varchar,
        encrypted_code -> Bytea,
        expiration_time -> Timestamptz,
        created_at -> Timestamptz,
        is_reset -> Bool,
    }
}

diesel::table! {
    platform_email_verifications (id) {
        id -> Int4,
        platform_user_id -> Uuid,
        verification_code -> Uuid,
        is_verified -> Bool,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
        expires_at -> Timestamptz,
    }
}

diesel::table! {
    platform_invite_codes (id) {
        id -> Int4,
        code -> Uuid,
    }
}

diesel::table! {
    platform_password_reset_requests (id) {
        id -> Int4,
        platform_user_id -> Uuid,
        #[max_length = 255]
        hashed_secret -> Varchar,
        encrypted_code -> Bytea,
        expiration_time -> Timestamptz,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
        is_reset -> Bool,
    }
}

diesel::table! {
    platform_users (id) {
        id -> Int4,
        uuid -> Uuid,
        email -> Citext,
        name -> Nullable<Text>,
        password_enc -> Nullable<Bytea>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    project_settings (id) {
        id -> Int4,
        project_id -> Int4,
        category -> Text,
        settings -> Jsonb,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    push_devices (id) {
        id -> Int8,
        uuid -> Uuid,
        user_id -> Uuid,
        installation_id -> Uuid,
        platform -> Text,
        provider -> Text,
        environment -> Text,
        app_id -> Text,
        push_token_enc -> Bytea,
        push_token_hash -> Bytea,
        notification_public_key -> Bytea,
        key_algorithm -> Text,
        supports_encrypted_preview -> Bool,
        supports_background_processing -> Bool,
        last_seen_at -> Timestamptz,
        revoked_at -> Nullable<Timestamptz>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    reasoning_items (id) {
        id -> Int8,
        uuid -> Uuid,
        conversation_id -> Int8,
        response_id -> Nullable<Int8>,
        assistant_message_id -> Nullable<Int8>,
        user_id -> Uuid,
        content_enc -> Nullable<Bytea>,
        summary_enc -> Nullable<Bytea>,
        reasoning_tokens -> Int4,
        status -> Text,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::ResponseStatus;

    responses (id) {
        id -> Int8,
        uuid -> Uuid,
        user_id -> Uuid,
        conversation_id -> Int8,
        status -> ResponseStatus,
        model -> Text,
        temperature -> Nullable<Float4>,
        top_p -> Nullable<Float4>,
        max_output_tokens -> Nullable<Int4>,
        tool_choice -> Nullable<Text>,
        parallel_tool_calls -> Bool,
        store -> Bool,
        metadata_enc -> Nullable<Bytea>,
        created_at -> Timestamptz,
        completed_at -> Nullable<Timestamptz>,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    token_usage (id) {
        id -> Int8,
        user_id -> Uuid,
        input_tokens -> Int4,
        output_tokens -> Int4,
        estimated_cost -> Numeric,
        created_at -> Timestamptz,
    }
}

diesel::table! {
    tool_calls (id) {
        id -> Int8,
        uuid -> Uuid,
        conversation_id -> Int8,
        response_id -> Nullable<Int8>,
        user_id -> Uuid,
        name -> Text,
        arguments_enc -> Nullable<Bytea>,
        argument_tokens -> Int4,
        status -> Text,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    tool_outputs (id) {
        id -> Int8,
        uuid -> Uuid,
        conversation_id -> Int8,
        response_id -> Nullable<Int8>,
        user_id -> Uuid,
        tool_call_fk -> Int8,
        output_enc -> Bytea,
        output_tokens -> Int4,
        status -> Text,
        error -> Nullable<Text>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    user_api_keys (id) {
        id -> Int4,
        user_id -> Uuid,
        #[max_length = 64]
        key_hash -> Varchar,
        name -> Text,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    user_embeddings (id) {
        id -> Int8,
        uuid -> Uuid,
        user_id -> Uuid,
        source_type -> Text,
        user_message_id -> Nullable<Int8>,
        assistant_message_id -> Nullable<Int8>,
        conversation_id -> Nullable<Int8>,
        vector_enc -> Bytea,
        embedding_model -> Text,
        vector_dim -> Int4,
        content_enc -> Bytea,
        metadata_enc -> Nullable<Bytea>,
        tags_enc -> Array<Nullable<Text>>,
        token_count -> Int4,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    user_instructions (id) {
        id -> Int8,
        uuid -> Uuid,
        user_id -> Uuid,
        name_enc -> Nullable<Bytea>,
        prompt_enc -> Bytea,
        prompt_tokens -> Int4,
        is_default -> Bool,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
        project_id -> Nullable<Int8>,
    }
}

diesel::table! {
    user_kv (id) {
        id -> Int8,
        user_id -> Uuid,
        key_enc -> Bytea,
        value_enc -> Bytea,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    user_messages (id) {
        id -> Int8,
        uuid -> Uuid,
        conversation_id -> Int8,
        response_id -> Nullable<Int8>,
        user_id -> Uuid,
        content_enc -> Bytea,
        prompt_tokens -> Int4,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
        attachment_text_enc -> Nullable<Bytea>,
        assistant_reaction -> Nullable<Text>,
    }
}

diesel::table! {
    user_oauth_connections (id) {
        id -> Int4,
        user_id -> Uuid,
        provider_id -> Int4,
        #[max_length = 255]
        provider_user_id -> Varchar,
        access_token_enc -> Bytea,
        refresh_token_enc -> Nullable<Bytea>,
        expires_at -> Nullable<Timestamptz>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    user_preferences (id) {
        id -> Int8,
        user_id -> Uuid,
        key -> Text,
        value_enc -> Bytea,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    user_seed_wrappings (id) {
        id -> Int8,
        user_id -> Uuid,
        credential_kind -> Text,
        credential_lookup_hash -> Bytea,
        wrapping_version -> Int2,
        seed_enc -> Bytea,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    users (id) {
        id -> Int4,
        uuid -> Uuid,
        name -> Nullable<Text>,
        email -> Nullable<Citext>,
        password_enc -> Nullable<Bytea>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
        project_id -> Int4,
    }
}

diesel::joinable!(agent_schedule_runs -> agent_schedules (schedule_id));
diesel::joinable!(agent_schedule_runs -> agents (agent_id));
diesel::joinable!(agent_schedules -> agents (agent_id));
diesel::joinable!(agents -> conversations (conversation_id));
diesel::joinable!(assistant_messages -> conversations (conversation_id));
diesel::joinable!(assistant_messages -> responses (response_id));
diesel::joinable!(conversation_summaries -> conversations (conversation_id));
diesel::joinable!(conversations -> conversation_projects (project_id));
diesel::joinable!(invite_codes -> orgs (org_id));
diesel::joinable!(notification_deliveries -> notification_events (event_id));
diesel::joinable!(notification_deliveries -> push_devices (push_device_id));
diesel::joinable!(notification_events -> org_projects (project_id));
diesel::joinable!(org_memberships -> orgs (org_id));
diesel::joinable!(org_project_secrets -> org_projects (project_id));
diesel::joinable!(org_projects -> orgs (org_id));
diesel::joinable!(project_settings -> org_projects (project_id));
diesel::joinable!(reasoning_items -> assistant_messages (assistant_message_id));
diesel::joinable!(reasoning_items -> conversations (conversation_id));
diesel::joinable!(reasoning_items -> responses (response_id));
diesel::joinable!(responses -> conversations (conversation_id));
diesel::joinable!(tool_calls -> conversations (conversation_id));
diesel::joinable!(tool_calls -> responses (response_id));
diesel::joinable!(tool_outputs -> conversations (conversation_id));
diesel::joinable!(tool_outputs -> responses (response_id));
diesel::joinable!(tool_outputs -> tool_calls (tool_call_fk));
diesel::joinable!(user_embeddings -> assistant_messages (assistant_message_id));
diesel::joinable!(user_embeddings -> conversations (conversation_id));
diesel::joinable!(user_embeddings -> user_messages (user_message_id));
diesel::joinable!(user_instructions -> conversation_projects (project_id));
diesel::joinable!(user_messages -> conversations (conversation_id));
diesel::joinable!(user_messages -> responses (response_id));
diesel::joinable!(user_oauth_connections -> oauth_providers (provider_id));
diesel::joinable!(users -> org_projects (project_id));

diesel::allow_tables_to_appear_in_same_query!(
    account_deletion_requests,
    agent_schedule_runs,
    agent_schedules,
    agents,
    app_data_migrations,
    assistant_messages,
    conversation_projects,
    conversation_summaries,
    conversations,
    email_opt_outs,
    email_verifications,
    enclave_secrets,
    invite_codes,
    memory_blocks,
    notification_deliveries,
    notification_events,
    oauth_providers,
    org_memberships,
    org_project_secrets,
    org_projects,
    orgs,
    password_reset_requests,
    platform_email_verifications,
    platform_invite_codes,
    platform_password_reset_requests,
    platform_users,
    project_settings,
    push_devices,
    reasoning_items,
    responses,
    token_usage,
    tool_calls,
    tool_outputs,
    user_api_keys,
    user_embeddings,
    user_instructions,
    user_kv,
    user_messages,
    user_oauth_connections,
    user_preferences,
    user_seed_wrappings,
    users,
);
