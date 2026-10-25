//! Offline tests for Maple routing, cache boundaries, and usage identity.
use super::*;
use crate::provider_registry::{MapleInstanceId, DUAL_INSTANCE_TEST_REGISTRY};

#[test]
fn uncensored_completion_access_uses_the_existing_pro_plan_gate() {
    let model = crate::model_config::DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID;
    assert!(matches!(
        ensure_completion_model_access(model, ModelPlan::Free),
        Err(ApiError::ModelNotAvailableOnPlan)
    ));
    assert!(ensure_completion_model_access(model, ModelPlan::Paid).is_ok());
}

#[test]
fn nested_and_pinned_completion_contexts_preserve_plan_without_provider_flag() {
    let cache = CompletionCachePolicy::LegacyV1;
    let caller = InferenceRoutingContext::new(ModelPlan::Paid);
    let child = caller.with_model_plan(ModelPlan::Free);
    assert_eq!(child.model_plan(), ModelPlan::Free);
    assert_eq!(caller.model_plan(), ModelPlan::Paid);

    for provider in [
        ProviderId::Maple(MapleInstanceId::I01),
        ProviderId::Maple(MapleInstanceId::I02),
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
            BillingContext::new(AuthMethod::ApiKey, public_model.to_string()),
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
    let cache = CompletionCachePolicy::BoundV2(derive_provider_cache_namespaces(
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

    for instance in MapleInstanceId::ALL {
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
        apply_provider_managed_request_fields(
            &mut body,
            route.provider.family_name(),
            user,
            &cache,
        );
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
            true,
            route.provider.family_name().to_owned(),
            route.public_model_id,
        );
        assert_eq!(event.provider_name, "maple");
        assert_eq!(event.model_name, model);
        assert_eq!(event.cached_input_tokens, Some(42));
        assert!(event.is_api_request);
    }
    assert_eq!(route_keys.len(), 2);
}

#[test]
fn direct_maple_cache_salt_is_private_and_provider_fields_are_rebuilt() {
    let user = Uuid::from_u128(1);
    let other_user = Uuid::from_u128(2);
    let root = CacheNamespaceRoot::from_bytes([0x42; 32]);
    let namespaces = derive_provider_cache_namespaces(&root, user);
    let policies = [
        CompletionCachePolicy::LegacyV1,
        CompletionCachePolicy::BoundV2(namespaces),
    ];
    for policy in policies {
        let expected = match &policy {
            CompletionCachePolicy::LegacyV1 => legacy_maple_user_cache_secret(user),
            CompletionCachePolicy::BoundV2(namespace) => namespace.maple_user_cache_secret(),
        };
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
                user,
                &policy,
            );
            // Dedicated endpoints expose native vLLM, which uses cache_salt.
            // user_cache_secret belongs to Tinfoil's shared-router contract.
            assert_eq!(body.get("cache_salt"), Some(&json!(expected)));
            assert!(!body.contains_key("user_cache_secret"));
            assert_eq!(body.get("model"), Some(&json!("deepseek-v4-1-flash")));
            assert_eq!(body.get("temperature"), Some(&json!(0.5)));

            let mut other_body = body.clone();
            let other_policy = match &policy {
                CompletionCachePolicy::LegacyV1 => CompletionCachePolicy::LegacyV1,
                CompletionCachePolicy::BoundV2(_) => CompletionCachePolicy::BoundV2(
                    derive_provider_cache_namespaces(&root, other_user),
                ),
            };
            apply_provider_managed_request_fields(
                &mut other_body,
                ProviderId::Maple(MapleInstanceId::I01).family_name(),
                other_user,
                &other_policy,
            );
            assert_ne!(other_body.get("cache_salt"), body.get("cache_salt"));

            // Provider field normalization is independent of model identity.
            // Rebuilding fields must never retain another provider's salt;
            // this does not authorize fallback between different checkpoints.
            apply_provider_managed_request_fields(
                &mut body,
                ProviderId::Tinfoil.as_str(),
                user,
                &policy,
            );
            let shared_expected = match &policy {
                CompletionCachePolicy::LegacyV1 => tinfoil_user_cache_secret(user),
                CompletionCachePolicy::BoundV2(namespace) => namespace.tinfoil_user_cache_secret(),
            };
            assert_eq!(body.get("user_cache_secret"), Some(&json!(shared_expected)));
            assert_ne!(body.get("user_cache_secret"), Some(&json!(expected)));
            assert!(!body.contains_key("cache_salt"));

            apply_provider_managed_request_fields(
                &mut body,
                ProviderId::Continuum.as_str(),
                user,
                &policy,
            );
            let continuum_expected = match &policy {
                CompletionCachePolicy::LegacyV1 => legacy_continuum_cache_salt(user),
                CompletionCachePolicy::BoundV2(namespace) => namespace.continuum_cache_salt(),
            };
            assert!(!body.contains_key("user_cache_secret"));
            assert_eq!(body.get("cache_salt"), Some(&json!(continuum_expected)));
            assert_ne!(body.get("cache_salt"), Some(&json!(expected)));
        }
    }
}
