//! Offline tests for Maple routing, cache boundaries, and usage identity.
use super::*;
use crate::provider_registry::{
    MapleInstanceId, DUAL_INSTANCE_TEST_INSTANCES, DUAL_INSTANCE_TEST_REGISTRY,
};

#[test]
fn uncensored_completion_access_uses_the_existing_pro_plan_gate() {
    for model in [
        crate::model_config::DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID,
        crate::model_config::KIMI_K3_1_PRO_UNCENSORED_MODEL_ID,
    ] {
        assert!(matches!(
            ensure_completion_model_access(model, ModelPlan::Free),
            Err(ApiError::ModelNotAvailableOnPlan)
        ));
        assert!(ensure_completion_model_access(model, ModelPlan::Paid).is_ok());
    }
}

#[test]
fn nested_and_pinned_completion_contexts_preserve_plan_without_provider_flag() {
    let cache = CompletionCachePolicy(derive_provider_cache_namespaces(
        &CacheNamespaceRoot::from_bytes([0x42; 32]),
        Uuid::from_u128(17),
    ));
    let caller = InferenceRoutingContext::new(ModelPlan::Paid);
    let child = caller.with_model_plan(ModelPlan::Free);
    assert_eq!(child.model_plan(), ModelPlan::Free);
    assert_eq!(caller.model_plan(), ModelPlan::Paid);

    for provider in [
        ProviderId::Maple(MapleInstanceId::I01),
        ProviderId::Maple(MapleInstanceId::I02),
        ProviderId::Maple(MapleInstanceId::I03),
        ProviderId::Tinfoil,
    ] {
        let public_model = "deepseek-v4-1-flash";
        let pinned = PinnedCompletionRequest::new(
            InferenceIntent::new(
                Uuid::from_u128(1),
                public_model,
                public_model,
                ModelPlan::Paid,
                InferenceSurface::Responses,
                WorkloadClass::Interactive,
            ),
            SelectedProviderRoute {
                provider,
                proxy: ProxyConfig {
                    base_url: "https://dedicated.invalid".to_string(),
                    api_key: None,
                    provider_name: provider.as_str().to_string(),
                },
                public_model_id: public_model.to_string(),
                provider_model_id: public_model.to_string(),
                response_model_id: public_model.to_string(),
                bucket: None,
                selection_source: RouteSelectionSource::StaticSplit,
            },
        );
        let context = CompletionExecutionContext::for_pinned(
            BillingContext::new(AuthMethod::ApiKey, public_model.to_string(), None),
            &pinned,
            &cache,
        );
        assert_eq!(context.routing.model_plan(), ModelPlan::Paid);
    }
}

#[test]
fn maple_instance_routes_keep_shared_cache_namespace_and_billing_identity() {
    let user = Uuid::from_u128(17);
    let model = crate::model_config::DEEPSEEK_V4_1_FLASH_MODEL_ID;
    let cache = CompletionCachePolicy(derive_provider_cache_namespaces(
        &CacheNamespaceRoot::from_bytes([0x42; 32]),
        user,
    ));
    let intent = InferenceIntent::new(
        user,
        model,
        model,
        ModelPlan::Paid,
        InferenceSurface::ChatCompletions,
        WorkloadClass::Interactive,
    );
    let mut prior_salt = None;
    let mut route_keys = HashSet::new();

    for instance in DUAL_INSTANCE_TEST_INSTANCES {
        let proxies = ProxyRouter::new(
            "http://continuum.invalid".to_owned(),
            None,
            "https://tinfoil.invalid".to_owned(),
        )
        .with_maple_api_key(Some("synthetic-maple-key".to_owned()))
        .with_maple_readiness(crate::provider_client::MapleReadiness::for_test_instances(
            &[instance],
        ));
        let route = ProviderRouter::with_test_registry(&DUAL_INSTANCE_TEST_REGISTRY)
            .select_active_completion_route(&proxies, &intent)
            .unwrap();
        assert_eq!(route.provider, ProviderId::Maple(instance));
        assert_eq!(route.proxy.provider_name, route.provider.as_str());
        assert!(route_keys.insert(route.identity().route_key()));

        let mut body = serde_json::Map::new();
        body.insert("cache_salt".to_owned(), json!("caller-controlled"));
        apply_provider_managed_request_fields(&mut body, route.provider.family_name(), &cache);
        let salt = body.get("cache_salt").cloned().unwrap();
        assert_ne!(salt, json!("caller-controlled"));
        if let Some(prior) = prior_salt.replace(salt.clone()) {
            assert_eq!(
                salt, prior,
                "moving replicas preserves the user's namespace"
            );
        }

        let event = build_usage_event(
            user,
            CompletionUsage {
                prompt_tokens: 100,
                completion_tokens: 20,
                cached_prompt_tokens: Some(42),
            },
            BigDecimal::from(0),
            route.provider.family_name().to_owned(),
            &BillingContext::new(AuthMethod::ApiKey, route.public_model_id, None),
        );
        assert_eq!(event.provider_name, "maple");
        assert_eq!(event.model_name, model);
        assert_eq!(event.cached_input_tokens, Some(42));
        assert!(event.is_api_request);
    }
    assert_eq!(route_keys.len(), 2);
}

/// The production route of the uncensored Kimi model, end to end through the
/// request rewrite a Maple send applies and the usage event billing prices.
#[test]
fn uncensored_kimi_route_sends_its_own_model_name_and_bills_the_maple_family() {
    let user = Uuid::from_u128(17);
    let model = crate::model_config::KIMI_K3_1_PRO_UNCENSORED_MODEL_ID;
    assert_eq!(model, "kimi-k3-1-pro-uncensored");
    let cache = CompletionCachePolicy(derive_provider_cache_namespaces(
        &CacheNamespaceRoot::from_bytes([0x42; 32]),
        user,
    ));
    let proxies = ProxyRouter::new(
        "http://continuum.invalid".to_owned(),
        None,
        "https://tinfoil.invalid".to_owned(),
    )
    .with_maple_api_key(Some("synthetic-maple-key".to_owned()))
    .with_maple_readiness(crate::provider_client::MapleReadiness::for_test_instances(
        &[MapleInstanceId::I03],
    ));

    for (surface, auth) in [
        (InferenceSurface::ChatCompletions, AuthMethod::ApiKey),
        (InferenceSurface::Responses, AuthMethod::Jwt),
    ] {
        let intent = InferenceIntent::new(
            user,
            model,
            model,
            ModelPlan::Paid,
            surface,
            WorkloadClass::Interactive,
        );
        let route = ProviderRouter::default()
            .select_active_completion_route(&proxies, &intent)
            .unwrap();
        assert_eq!(route.provider, ProviderId::Maple(MapleInstanceId::I03));
        assert_eq!(route.proxy.provider_name, "maple-i03");
        assert_eq!(route.provider.family_name(), "maple");
        assert_eq!(route.public_model_id, model);
        assert_eq!(route.provider_model_id, model);
        assert_eq!(route.response_model_id, model);

        // The same rewrite order a send applies: provider model, backend-owned
        // cache namespace, then the dedicated node's engine-policy sanitizer.
        let mut body = json!({
            "model": model,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "synthetic"},
                {"type": "image_url", "uuid": "caller-cache-entry", "image_url": {
                    "url": "data:image/png;base64,AA=="
                }}
            ]}],
            "reasoning_effort": "high",
            "temperature": 1.0,
            "top_p": 0.95,
            "chat_template_kwargs": {"preserve_thinking": true},
            "cache_salt": "caller-controlled",
            "user_cache_secret": "caller-controlled",
            "priority": -1000,
            "stream": true
        })
        .as_object()
        .unwrap()
        .clone();
        body.insert("model".to_owned(), json!(route.provider_model_id));
        apply_provider_managed_request_fields(&mut body, route.provider.family_name(), &cache);
        ensure_stream_usage(&mut body);
        let mut body = Value::Object(body);
        assert!(route.provider.is_maple());
        crate::maple_inference::sanitize_completion_request(&mut body);

        assert_eq!(body["model"], "kimi-k3-1-pro-uncensored");
        let CompletionCachePolicy(namespaces) = &cache;
        assert_eq!(
            body["cache_salt"],
            json!(namespaces.maple_user_cache_secret())
        );
        assert!(body.get("user_cache_secret").is_none());
        assert!(body.get("priority").is_none());
        assert!(body["messages"][0]["content"][1].get("uuid").is_none());
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["temperature"], 1.0);
        assert_eq!(body["top_p"], 0.95);
        assert_eq!(
            body["chat_template_kwargs"],
            json!({"preserve_thinking": true})
        );
        assert_eq!(body["stream_options"], json!({"include_usage": true}));

        let event = build_usage_event(
            user,
            CompletionUsage {
                prompt_tokens: 100,
                completion_tokens: 20,
                cached_prompt_tokens: Some(42),
            },
            BigDecimal::from(0),
            route.provider.family_name().to_owned(),
            &BillingContext::new(auth, route.public_model_id.clone(), None),
        );
        assert_eq!(event.provider_name, "maple");
        assert_eq!(event.model_name, "kimi-k3-1-pro-uncensored");
        assert_eq!(event.input_tokens, 100);
        assert_eq!(event.output_tokens, 20);
        assert_eq!(event.cached_input_tokens, Some(42));
        assert_eq!(event.is_api_request, matches!(auth, AuthMethod::ApiKey));

        // The node's own name for the model is already the public one, and a
        // response naming anything else is still returned under the public id.
        let mut response = json!({"model": route.provider_model_id, "choices": []});
        canonicalize_response_model(&mut response, &route.response_model_id);
        assert_eq!(response["model"], "kimi-k3-1-pro-uncensored");
    }
}

#[test]
fn direct_maple_cache_salt_is_private_and_provider_fields_are_rebuilt() {
    let user = Uuid::from_u128(1);
    let other_user = Uuid::from_u128(2);
    let root = CacheNamespaceRoot::from_bytes([0x42; 32]);
    let namespaces = derive_provider_cache_namespaces(&root, user);
    let expected = namespaces.maple_user_cache_secret();
    let shared_expected = namespaces.tinfoil_user_cache_secret();
    let continuum_expected = namespaces.continuum_cache_salt();
    let policy = CompletionCachePolicy(namespaces);
    let other_policy = CompletionCachePolicy(derive_provider_cache_namespaces(&root, other_user));
    for untrusted in [
        json!("caller-controlled"),
        json!(null),
        json!({"other_user": "injected"}),
        json!(["injected"]),
        json!(123),
        json!(false),
    ] {
        let mut body = json!({
            "model": "deepseek-v4-1-flash",
            "messages": [{"role": "user", "content": "synthetic cache boundary test"}],
            "user_cache_secret": untrusted,
            "cache_salt": untrusted,
            "temperature": 0.5
        })
        .as_object()
        .unwrap()
        .clone();
        apply_provider_managed_request_fields(
            &mut body,
            ProviderId::Maple(MapleInstanceId::I01).family_name(),
            &policy,
        );
        // Dedicated endpoints expose native vLLM, which uses cache_salt.
        // user_cache_secret belongs to Tinfoil's shared-router contract.
        assert_eq!(body.get("cache_salt"), Some(&json!(expected)));
        assert!(!body.contains_key("user_cache_secret"));
        assert_eq!(body.get("model"), Some(&json!("deepseek-v4-1-flash")));
        assert_eq!(body.get("temperature"), Some(&json!(0.5)));

        let mut other_body = body.clone();
        apply_provider_managed_request_fields(
            &mut other_body,
            ProviderId::Maple(MapleInstanceId::I01).family_name(),
            &other_policy,
        );
        assert_ne!(other_body.get("cache_salt"), body.get("cache_salt"));

        // Provider field normalization is independent of model identity.
        // Rebuilding fields must never retain another provider's salt;
        // this does not authorize fallback between different checkpoints.
        apply_provider_managed_request_fields(&mut body, ProviderId::Tinfoil.as_str(), &policy);
        assert_eq!(body.get("user_cache_secret"), Some(&json!(shared_expected)));
        assert_ne!(body.get("user_cache_secret"), Some(&json!(expected)));
        assert!(!body.contains_key("cache_salt"));

        apply_provider_managed_request_fields(&mut body, ProviderId::Continuum.as_str(), &policy);
        assert!(!body.contains_key("user_cache_secret"));
        assert_eq!(body.get("cache_salt"), Some(&json!(continuum_expected)));
        assert_ne!(body.get("cache_salt"), Some(&json!(expected)));
    }
}
