//! Caller-owned identities for Maple's dedicated attested inference endpoint.
//!
//! Changing a node's measured release requires reviewing these pins before the
//! node update. DNS, the API credential, or a server-supplied release reference
//! never authorizes a new measurement. This is ordinary attested HTTPS using
//! Tinfoil's direct V3 path; it does not use shared-router discovery.

use crate::provider_registry::MapleInstanceId;
use tinfoil::{ApprovedRelease, DirectPolicy, Measurement, PinnedRelease, PredicateType};

/// Backend-owned identity, address, and approved release set for one guest.
/// Each guest has an independent verifier/client even when its release pins are
/// identical to a replica's. Neither DNS nor request metadata selects this set.
pub(crate) struct MapleEndpoint {
    pub(crate) instance: MapleInstanceId,
    pub(crate) host: &'static str,
    pub(crate) base_url: &'static str,
    current_release: Option<&'static ReleasePin>,
    staged_releases: &'static [ReleasePin],
}

const I01_ENDPOINT: MapleEndpoint = MapleEndpoint {
    instance: MapleInstanceId::I01,
    host: "i01.tfinf.maple.ai",
    base_url: "https://i01.tfinf.maple.ai",
    current_release: Some(&I01_CURRENT_RELEASE),
    staged_releases: &[],
};
const I02_ENDPOINT: MapleEndpoint = MapleEndpoint {
    instance: MapleInstanceId::I02,
    host: "i02.tfinf.maple.ai",
    base_url: "https://i02.tfinf.maple.ai",
    current_release: Some(&I02_CURRENT_RELEASE),
    staged_releases: &[],
};

pub(crate) const fn endpoint(instance: MapleInstanceId) -> &'static MapleEndpoint {
    match instance {
        MapleInstanceId::I01 => &I01_ENDPOINT,
        MapleInstanceId::I02 => &I02_ENDPOINT,
    }
}

/// Remove caller-controlled engine policy from Maple's direct vLLM requests.
///
/// vLLM reads media cache identifiers from the content part's own `uuid`,
/// independently of the backend-owned KV cache salt. Do not recursively remove
/// UUIDs: tool arguments, schemas, text, and nested application data belong to
/// the caller. This follows the media parser in vLLM db9527a4, including its
/// shorthand media fields without `type` and media fields paired with a UUID
/// under another type.
pub(crate) fn sanitize_completion_request(body: &mut serde_json::Value) {
    let Some(body) = body.as_object_mut() else {
        return;
    };
    body.remove("priority");

    let Some(messages) = body
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for message in messages {
        let Some(parts) = message
            .get_mut("content")
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for part in parts {
            let Some(part) = part.as_object_mut() else {
                continue;
            };
            let is_media = matches!(
                part.get("type").and_then(serde_json::Value::as_str),
                Some(
                    "image_url"
                        | "input_image"
                        | "image_pil"
                        | "image_embeds"
                        | "audio_url"
                        | "input_audio"
                        | "audio_embeds"
                        | "video_url"
                        | "video_embeds"
                )
            ) || [
                "image_url",
                "image_pil",
                "image_embeds",
                "audio_url",
                "input_audio",
                "audio_embeds",
                "video_url",
                "video_embeds",
            ]
            .iter()
            .any(|field| part.contains_key(*field));
            if is_media {
                part.remove("uuid");
            }
        }
    }
}

struct ReleasePin {
    repo: &'static str,
    tag: &'static str,
    digest: &'static str,
    registers: &'static [&'static str],
}

// Keep each guest's release pins independent, even while both run the same
// artifact. A staged i02 update must never broaden i01's attestation policy.
// These identify the published artifact and full multiplatform measurement,
// not a cached live quote. Remove retired entries after rollout completes.
// Prepared for the next uncensored guest; it has no customer route yet.
const I01_CURRENT_RELEASE: ReleasePin = ReleasePin {
    repo: "MaplePrivacyLabs/inference-deepseek-uncensored",
    tag: "v0.1.0+b300",
    digest: "6a8e906bc31caaccf5b6bbe84dec99c7213ec1d77aa360df464d531744590ac2",
    registers: &[
        "9e01436661dff4702b622a51a7bbcb83ce0a61d0c2bdc2de5a2492fc1dec70bb1997df829147a154e11fdfc6dbe044fe",
        "758adac43a82a5f3146342660caefea0a7714414dd9055c4a56d1502273d85bbad86d625096a206c9335c23d05df3f99",
        "898884507ffe6bb1d51aa3bf548d8748043aa8e2018d545424c312e7a39109d75f30760f0d4124ae2cf5c89cf07db8dd",
    ],
};

// Qualified independently through the official CLI and the pinned Rust SDK.
// Stock i02 measurements are deliberately absent from this model's policy.
const I02_CURRENT_RELEASE: ReleasePin = ReleasePin {
    repo: "MaplePrivacyLabs/inference-deepseek-uncensored",
    tag: "v0.1.0-rc.1+b300",
    digest: "1d92cd06f7b47179bf81612f6a41fdb0a329253affef1803f0cdeb5175cc80c4",
    registers: &[
        "b391ea9621062080c01c3686bb70e8889148f6d6c7ca71eec6c149f4132b50f7b47e251f113a9a0861f05e546203aa39",
        "7e57f9877b4533e7dcb8dd7419dfaf422f3c7cef6fd337a4eaf27ae90a751ff04945ef0d4fb6c3753285cb1f09ce048e",
        "a47f4e2707c76f799513b5ca277982d1988891bf486867ad1de56ef028e29a8e54f37c4bbf9fa70cab6ca9f1683acef7",
    ],
};

/// An unconfigured guest has no verifier or network initialization. This is
/// distinct from a malformed configured policy, which remains a startup error.
pub(crate) fn direct_policy(
    instance: MapleInstanceId,
) -> Result<Option<DirectPolicy>, tinfoil::Error> {
    let approved = approved_releases(endpoint(instance));
    if approved.is_empty() {
        return Ok(None);
    }
    DirectPolicy::new(approved).map(Some)
}

fn approved_releases(endpoint: &MapleEndpoint) -> Vec<ApprovedRelease> {
    endpoint
        .current_release
        .into_iter()
        .chain(endpoint.staged_releases.iter())
        .map(|pin| ApprovedRelease {
            release: PinnedRelease {
                repo: pin.repo.to_owned(),
                tag: pin.tag.to_owned(),
                digest: pin.digest.to_owned(),
            },
            measurement: Measurement {
                type_: PredicateType::SnpTdxMultiPlatformV1,
                registers: pin
                    .registers
                    .iter()
                    .map(|register| (*register).to_owned())
                    .collect(),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn completion_policy_strips_priority_without_changing_sampling_or_tool_data() {
        let mut body = json!({
            "model": "deepseek-v4-1-flash",
            "priority": -1000,
            "temperature": 0.7,
            "top_p": 0.9,
            "max_tokens": 2048,
            "stream": true,
            "stream_options": {"include_usage": true},
            "cache_salt": "backend-owned",
            "reasoning_effort": "high",
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "record",
                    "schema": {"properties": {"uuid": {"type": "string"}}}
                }
            },
            "tools": [{"type": "function", "function": {
                "name": "lookup",
                "parameters": {"properties": {
                    "priority": {"type": "number"}, "uuid": {"type": "string"}
                }}
            }}],
            "messages": [
                {"role": "user", "uuid": "message-id", "content": [
                    {"type": "text", "text": "Use uuid and priority", "uuid": "text-metadata"}
                ]},
                {"role": "assistant", "tool_calls": [{"type": "function", "function": {
                    "name": "lookup", "arguments": "{\"uuid\":\"record-id\",\"priority\":1}"
                }}]},
                {"role": "tool", "tool_call_id": "call-1", "content": "{\"uuid\":\"record-id\"}"}
            ]
        });
        let mut expected = body.clone();
        expected.as_object_mut().unwrap().remove("priority");

        sanitize_completion_request(&mut body);

        assert_eq!(body, expected);
    }

    #[test]
    fn completion_policy_removes_identifiers_from_typed_media_parts() {
        for part in [
            json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,AA==", "detail": "auto"}}),
            json!({"type": "input_image", "image_url": "data:image/png;base64,AA=="}),
            json!({"type": "image_pil", "image_pil": null}),
            json!({"type": "image_embeds", "image_embeds": "tensor"}),
            json!({"type": "audio_url", "audio_url": {"url": "data:audio/wav;base64,AA=="}}),
            json!({"type": "input_audio", "input_audio": {"data": "AA==", "format": "wav"}}),
            json!({"type": "audio_embeds", "audio_embeds": "tensor"}),
            json!({"type": "video_url", "video_url": {"url": "data:video/mp4;base64,AA=="}}),
            json!({"type": "video_embeds", "video_embeds": "tensor"}),
        ] {
            let mut caller_part = part.clone();
            caller_part["uuid"] = json!("caller-chosen-cache-entry");
            let mut body = json!({"messages": [{"role": "user", "content": [caller_part]}]});

            sanitize_completion_request(&mut body);

            assert_eq!(body["messages"][0]["content"][0], part);
        }
    }

    #[test]
    fn completion_policy_covers_media_shorthand_and_uuid_type_override() {
        for media_field in [
            "image_url",
            "image_pil",
            "image_embeds",
            "audio_url",
            "input_audio",
            "audio_embeds",
            "video_url",
            "video_embeds",
        ] {
            for part_type in [None, Some("text"), Some("unrecognized")] {
                let mut part = json!({"uuid": "caller-cache-entry"});
                part[media_field] = Value::Null;
                if let Some(part_type) = part_type {
                    part["type"] = json!(part_type);
                }
                let mut expected = part.clone();
                expected.as_object_mut().unwrap().remove("uuid");
                let mut body = json!({"messages": [{"content": [part]}]});

                sanitize_completion_request(&mut body);

                assert_eq!(body["messages"][0]["content"][0], expected);
            }
        }
    }

    #[test]
    fn completion_policy_does_not_recursively_strip_application_uuids() {
        let mut body = json!({
            "uuid": "request-metadata",
            "messages": [{"content": [
                {"type": "image_url", "uuid": "engine-cache-entry", "image_url": {
                    "url": "data:image/png;base64,AA==", "uuid": "nested-metadata"
                }},
                {"type": "tool_reference", "name": "lookup", "uuid": "tool-metadata"},
                {"type": "application_data", "uuid": "ordinary-data", "arguments": {
                    "image_url": "application-value", "uuid": "argument-data"
                }}
            ]}]
        });
        let mut expected = body.clone();
        expected["messages"][0]["content"][0]
            .as_object_mut()
            .unwrap()
            .remove("uuid");

        sanitize_completion_request(&mut body);

        assert_eq!(body, expected);
    }

    #[test]
    fn completion_policy_leaves_unrelated_or_malformed_shapes_to_normal_validation() {
        for mut body in [
            Value::Null,
            json!([]),
            json!({"messages": null}),
            json!({"messages": [null, "text", {"content": "uuid"}, {"content": [null, "text"]}]}),
            json!({"input": [{"type": "function_call", "arguments": {"uuid": "record-id"}}]}),
        ] {
            let expected = body.clone();
            sanitize_completion_request(&mut body);
            assert_eq!(body, expected);
        }
    }

    #[test]
    fn dedicated_policies_are_complete_and_explicit_for_each_guest() {
        let mut hosts = std::collections::HashSet::new();
        for instance in MapleInstanceId::ALL {
            let endpoint = endpoint(instance);
            assert_eq!(endpoint.instance, instance);
            assert!(hosts.insert(endpoint.host));
            let policy = direct_policy(instance)
                .expect("compiled current/staged pins must form a valid policy");
            let serialized = format!("{policy:?}");
            for pin in endpoint
                .current_release
                .into_iter()
                .chain(endpoint.staged_releases.iter())
            {
                assert!(serialized.contains(pin.repo));
                assert!(serialized.contains(pin.tag));
                assert!(serialized.contains(pin.digest));
                for register in pin.registers {
                    assert!(serialized.contains(register));
                }
            }
            let url = reqwest::Url::parse(endpoint.base_url).unwrap();
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some(endpoint.host));
            assert!(url.port().is_none());
            assert_eq!(url.path(), "/");
        }
        assert_eq!(endpoint(MapleInstanceId::I01).host, "i01.tfinf.maple.ai");
        assert_eq!(endpoint(MapleInstanceId::I02).host, "i02.tfinf.maple.ai");
    }

    #[test]
    fn staging_i02_release_does_not_approve_it_for_i01() {
        // Synthetic release identity is local to this test; no endpoint uses it.
        const I02_TEST_STAGE: ReleasePin = ReleasePin {
            repo: I02_CURRENT_RELEASE.repo,
            tag: "test-i02-only-stage",
            digest: I02_CURRENT_RELEASE.digest,
            registers: I02_CURRENT_RELEASE.registers,
        };
        const STAGED_I02_ENDPOINT: MapleEndpoint = MapleEndpoint {
            instance: MapleInstanceId::I02,
            host: "i02.tfinf.maple.ai",
            base_url: "https://i02.tfinf.maple.ai",
            current_release: Some(&I02_CURRENT_RELEASE),
            staged_releases: &[I02_TEST_STAGE],
        };

        let i01_approved = approved_releases(endpoint(MapleInstanceId::I01));
        let current_i02_approved = approved_releases(endpoint(MapleInstanceId::I02));
        let staged_i02_approved = approved_releases(&STAGED_I02_ENDPOINT);
        assert_eq!(i01_approved.len(), 1);
        assert_eq!(current_i02_approved.len(), 1);
        assert_eq!(staged_i02_approved.len(), 2);
        assert_eq!(staged_i02_approved[1].release.tag, I02_TEST_STAGE.tag);
        assert!(i01_approved
            .iter()
            .all(|approved| approved.release.tag != I02_TEST_STAGE.tag));
        DirectPolicy::new(i01_approved).expect("i01 current policy remains valid");
        DirectPolicy::new(staged_i02_approved).expect("i02 staged policy remains valid");
    }

    #[test]
    fn uncensored_guest_approves_only_its_qualified_release() {
        let approved = approved_releases(endpoint(MapleInstanceId::I02));
        assert_eq!(approved.len(), 1);
        let release = &approved[0];
        assert_eq!(
            release.release.repo,
            "MaplePrivacyLabs/inference-deepseek-uncensored"
        );
        assert_eq!(release.release.tag, "v0.1.0-rc.1+b300");
        assert_eq!(
            release.release.digest,
            "1d92cd06f7b47179bf81612f6a41fdb0a329253affef1803f0cdeb5175cc80c4"
        );
        assert_eq!(
            release.measurement.type_,
            PredicateType::SnpTdxMultiPlatformV1
        );
        assert_eq!(release.measurement.registers, [
            "b391ea9621062080c01c3686bb70e8889148f6d6c7ca71eec6c149f4132b50f7b47e251f113a9a0861f05e546203aa39",
            "7e57f9877b4533e7dcb8dd7419dfaf422f3c7cef6fd337a4eaf27ae90a751ff04945ef0d4fb6c3753285cb1f09ce048e",
            "a47f4e2707c76f799513b5ca277982d1988891bf486867ad1de56ef028e29a8e54f37c4bbf9fa70cab6ca9f1683acef7",
        ]);
        assert!(approved.iter().all(|release| {
            release.release.tag != I01_CURRENT_RELEASE.tag
                && release.release.digest != I01_CURRENT_RELEASE.digest
                && release.measurement.registers[0] != I01_CURRENT_RELEASE.registers[0]
        }));
        assert!(direct_policy(MapleInstanceId::I02).unwrap().is_some());
        assert!(direct_policy(MapleInstanceId::I01).unwrap().is_some());
    }

    #[test]
    fn prepared_i01_policy_is_new_uncensored_only_and_never_authorizes_stock() {
        let prepared = approved_releases(endpoint(MapleInstanceId::I01));
        assert_eq!(prepared.len(), 1);
        assert_eq!(
            prepared[0].release.repo,
            "MaplePrivacyLabs/inference-deepseek-uncensored"
        );
        assert_eq!(prepared[0].release.tag, "v0.1.0+b300");
        assert_eq!(
            prepared[0].measurement.type_,
            PredicateType::SnpTdxMultiPlatformV1
        );
        assert_eq!(prepared[0].measurement.registers.len(), 3);
        // Neither the existing old uncensored guest nor the former stock guest
        // can satisfy this endpoint's new, exclusive release policy.
        assert_ne!(prepared[0].release.digest, I02_CURRENT_RELEASE.digest);
        assert_ne!(
            prepared[0].measurement.registers[0],
            I02_CURRENT_RELEASE.registers[0]
        );
        for instance in MapleInstanceId::ALL {
            for approved in approved_releases(endpoint(instance)) {
                assert_ne!(approved.release.repo, "MaplePrivacyLabs/inference-deepseek");
                assert_ne!(
                    approved.release.digest,
                    "7f421e8aefdd63181b71b9d202ac6a3b05b976c6ecb156354a20ff691931f5be"
                );
                assert_ne!(approved.measurement.registers[0], "f645aa2a679cf7ed575bb5d9d2fa18104d24ae545950d6c3669b6af1d81328f099f3437f9129c608ac9ff5a97927d365");
            }
        }
    }

    #[test]
    fn retired_release_is_not_approved_on_either_guest() {
        for instance in MapleInstanceId::ALL {
            for approved in approved_releases(endpoint(instance)) {
                assert_ne!(approved.release.tag, "v0.1.0");
                assert_ne!(
                    approved.release.digest,
                    "2d69e8544e5b2706d0e6004bc0ac925dc9abd79cb284f9400c26c9d145e25b00"
                );
                for retired_register in [
                    "d4d5fd1166413a3229a83899b99c06fb8cb904badb2ed5b9b8e6cb7414ad110628b6780eddaf10c42cbc996909145541",
                    "24e53d3e8a77e9aa9182b61bbddc5138050c5dc00628231c51cfe6798431f64d59af20494708190c7aca85b166675550",
                ] {
                    assert!(!approved.measurement.registers.iter().any(|register| register == retired_register));
                }
            }
        }
    }
}
