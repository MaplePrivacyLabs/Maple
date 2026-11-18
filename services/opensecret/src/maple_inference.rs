//! Caller-owned identities for Maple's dedicated attested inference endpoint.
//!
//! Changing a node's measured release requires reviewing these pins before the
//! node update. DNS, the API credential, or a server-supplied release reference
//! never authorizes a new measurement. This is ordinary attested HTTPS using
//! Tinfoil's direct V3 path; it does not use shared-router discovery.
//!
//! Tinfoil's verifier authenticates the hardware evidence and proves that the
//! guest runs a signed release of the pinned repository. [`MaplePolicy`] then
//! narrows that to the exact releases approved here.

use crate::provider_registry::MapleInstanceId;
use tinfoil::verifier::{Measurement, Register, Verification};

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
    /// The AMD SEV-SNP launch measurement of the release. Both guests are AMD
    /// hosts; a TDX guest is never approved, so no TDX registers are pinned.
    snp_launch: &'static str,
}

// Keep each guest's release pins independent, even while both run the same
// artifact. A staged i02 update must never broaden i01's attestation policy.
// These identify the published artifact and its launch measurement, not a
// cached live quote. Remove retired entries after rollout completes.
// i01 serves the uncensored model; i02 independently serves stock DeepSeek.
const I01_CURRENT_RELEASE: ReleasePin = ReleasePin {
    repo: "MaplePrivacyLabs/inference-deepseek-uncensored",
    tag: "v0.1.0+b300",
    digest: "6a8e906bc31caaccf5b6bbe84dec99c7213ec1d77aa360df464d531744590ac2",
    snp_launch: "9e01436661dff4702b622a51a7bbcb83ce0a61d0c2bdc2de5a2492fc1dec70bb1997df829147a154e11fdfc6dbe044fe",
};

// Only the new stock release is approved. The former uncensored i02 release
// must not become eligible through this guest's stock route.
const I02_CURRENT_RELEASE: ReleasePin = ReleasePin {
    repo: "MaplePrivacyLabs/inference-deepseek",
    tag: "v0.1.2",
    digest: "4bc5f2119cb55e125c9cc5c57e3fe120df98f8145dc675c8d201ac01882989ec",
    snp_launch: "3f35fb1e92c599564e85e2b9adae8778bf8a9f553a8c936132301f09d4aa31a4a505b1c8f96cef407c53f4df67d23030",
};

/// One release a guest may run: the repository that published it, the exact
/// artifact, and the launch measurement the hardware must report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ApprovedRelease {
    repo: &'static str,
    tag: &'static str,
    digest: &'static str,
    snp_launch: Register,
}

/// The complete set of releases one guest may run. It is built once from the
/// compiled pins and never changes, so initialization, recovery and renewal
/// all verify against the same list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MaplePolicy {
    approved: Vec<ApprovedRelease>,
}

/// A compiled pin that is not a usable release identity.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InvalidMaplePolicy;

/// The guest verified, but as a release this policy does not approve.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("verified release is not approved for this guest")]
pub(crate) struct ReleaseNotApproved;

impl MaplePolicy {
    fn new(
        pins: impl IntoIterator<Item = &'static ReleasePin>,
    ) -> Result<Self, InvalidMaplePolicy> {
        let approved = pins
            .into_iter()
            .map(|pin| {
                let well_formed = !pin.repo.is_empty()
                    && !pin.tag.is_empty()
                    && pin.digest.len() == 64
                    && pin
                        .digest
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
                let snp_launch = pin.snp_launch.parse::<Register>();
                match (well_formed, snp_launch) {
                    (true, Ok(snp_launch)) => Ok(ApprovedRelease {
                        repo: pin.repo,
                        tag: pin.tag,
                        digest: pin.digest,
                        snp_launch,
                    }),
                    _ => Err(InvalidMaplePolicy),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { approved })
    }

    /// The repositories to verify the guest against, in approval order and
    /// without repeats. The verifier pins one repository per attempt.
    pub(crate) fn repos(&self) -> Vec<&'static str> {
        let mut repos = Vec::new();
        for release in &self.approved {
            if !repos.contains(&release.repo) {
                repos.push(release.repo);
            }
        }
        repos
    }

    /// Accept a verification only when repository, tag, digest and the
    /// launch measurement the hardware signed all belong to one approved
    /// release. Fields of two different releases never combine.
    pub(crate) fn check(&self, verified: &Verification) -> Result<(), ReleaseNotApproved> {
        let Measurement::SevSnp { launch } = verified.enclave_measurement() else {
            return Err(ReleaseNotApproved);
        };
        self.check_identity(
            verified.config_repo(),
            verified.code_tag(),
            verified.code_digest(),
            launch,
        )
    }

    fn check_identity(
        &self,
        repo: &str,
        tag: &str,
        digest: &str,
        snp_launch: &Register,
    ) -> Result<(), ReleaseNotApproved> {
        let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
        self.approved
            .iter()
            .any(|release| {
                release.repo == repo
                    && release.tag == tag
                    && release.digest == digest
                    && release.snp_launch == *snp_launch
            })
            .then_some(())
            .ok_or(ReleaseNotApproved)
    }
}

/// An unconfigured guest has no verifier or network initialization. This is
/// distinct from a malformed configured policy, which remains a startup error.
pub(crate) fn direct_policy(
    instance: MapleInstanceId,
) -> Result<Option<MaplePolicy>, InvalidMaplePolicy> {
    let endpoint = endpoint(instance);
    let policy = MaplePolicy::new(
        endpoint
            .current_release
            .into_iter()
            .chain(endpoint.staged_releases.iter()),
    )?;
    Ok((!policy.approved.is_empty()).then_some(policy))
}

#[cfg(test)]
pub(crate) fn empty_policy_for_test() -> MaplePolicy {
    MaplePolicy {
        approved: Vec::new(),
    }
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

    fn pins(endpoint: &MapleEndpoint) -> Vec<&'static ReleasePin> {
        endpoint
            .current_release
            .into_iter()
            .chain(endpoint.staged_releases.iter())
            .collect()
    }

    fn policy_for(endpoint: &MapleEndpoint) -> MaplePolicy {
        MaplePolicy::new(pins(endpoint)).expect("compiled pins must form a valid policy")
    }

    fn approves(policy: &MaplePolicy, pin: &ReleasePin) -> bool {
        policy
            .check_identity(
                pin.repo,
                pin.tag,
                pin.digest,
                &pin.snp_launch.parse().expect("test launch measurement"),
            )
            .is_ok()
    }

    #[test]
    fn dedicated_policies_are_complete_and_explicit_for_each_guest() {
        let mut hosts = std::collections::HashSet::new();
        for instance in MapleInstanceId::ALL {
            let endpoint = endpoint(instance);
            assert_eq!(endpoint.instance, instance);
            assert!(hosts.insert(endpoint.host));
            let policy = direct_policy(instance)
                .expect("compiled current/staged pins must form a valid policy")
                .expect("both guests are configured");
            let serialized = format!("{policy:?}");
            for pin in pins(endpoint) {
                assert!(serialized.contains(pin.repo));
                assert!(serialized.contains(pin.tag));
                assert!(serialized.contains(pin.digest));
                assert!(serialized.contains(pin.snp_launch));
                assert!(approves(&policy, pin));
                assert!(policy.repos().contains(&pin.repo));
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
    fn a_release_is_approved_only_as_a_whole() {
        let policy = policy_for(endpoint(MapleInstanceId::I02));
        let pin = &I02_CURRENT_RELEASE;
        let launch: Register = pin.snp_launch.parse().unwrap();
        let other_launch: Register = I01_CURRENT_RELEASE.snp_launch.parse().unwrap();

        assert!(policy
            .check_identity(pin.repo, pin.tag, pin.digest, &launch)
            .is_ok());
        // The verifier may report the digest with its algorithm prefix.
        assert!(policy
            .check_identity(
                pin.repo,
                pin.tag,
                &format!("sha256:{}", pin.digest),
                &launch
            )
            .is_ok());

        for (repo, tag, digest, launch) in [
            (I01_CURRENT_RELEASE.repo, pin.tag, pin.digest, &launch),
            (pin.repo, "v0.1.1", pin.digest, &launch),
            (pin.repo, pin.tag, I01_CURRENT_RELEASE.digest, &launch),
            (pin.repo, pin.tag, pin.digest, &other_launch),
            (pin.repo, pin.tag, "", &launch),
            ("", "", "", &launch),
        ] {
            assert_eq!(
                policy.check_identity(repo, tag, digest, launch),
                Err(ReleaseNotApproved)
            );
        }
    }

    #[test]
    fn malformed_pins_are_a_policy_error_and_no_pins_are_no_policy() {
        const BAD_LAUNCH: ReleasePin = ReleasePin {
            repo: I02_CURRENT_RELEASE.repo,
            tag: I02_CURRENT_RELEASE.tag,
            digest: I02_CURRENT_RELEASE.digest,
            snp_launch: "not-a-measurement",
        };
        const BAD_DIGEST: ReleasePin = ReleasePin {
            repo: I02_CURRENT_RELEASE.repo,
            tag: I02_CURRENT_RELEASE.tag,
            digest: "sha256:4bc5f2119cb55e125c9cc5c57e3fe120df98f8145dc675c8d201ac01882989ec",
            snp_launch: I02_CURRENT_RELEASE.snp_launch,
        };
        const NO_REPO: ReleasePin = ReleasePin {
            repo: "",
            tag: I02_CURRENT_RELEASE.tag,
            digest: I02_CURRENT_RELEASE.digest,
            snp_launch: I02_CURRENT_RELEASE.snp_launch,
        };
        for pin in [&BAD_LAUNCH, &BAD_DIGEST, &NO_REPO] {
            assert_eq!(MaplePolicy::new([pin]), Err(InvalidMaplePolicy));
        }
        assert!(MaplePolicy::new([]).unwrap().repos().is_empty());
    }

    #[test]
    fn staging_i02_release_does_not_approve_it_for_i01() {
        // Synthetic release identity is local to this test; no endpoint uses it.
        const I02_TEST_STAGE: ReleasePin = ReleasePin {
            repo: "MaplePrivacyLabs/test-i02-only-stage",
            tag: "test-i02-only-stage",
            digest: I02_CURRENT_RELEASE.digest,
            snp_launch: I02_CURRENT_RELEASE.snp_launch,
        };
        const STAGED_I02_ENDPOINT: MapleEndpoint = MapleEndpoint {
            instance: MapleInstanceId::I02,
            host: "i02.tfinf.maple.ai",
            base_url: "https://i02.tfinf.maple.ai",
            current_release: Some(&I02_CURRENT_RELEASE),
            staged_releases: &[I02_TEST_STAGE],
        };

        let i01 = policy_for(endpoint(MapleInstanceId::I01));
        let current_i02 = policy_for(endpoint(MapleInstanceId::I02));
        let staged_i02 = policy_for(&STAGED_I02_ENDPOINT);
        assert_eq!(i01.approved.len(), 1);
        assert_eq!(current_i02.approved.len(), 1);
        assert_eq!(staged_i02.approved.len(), 2);
        // The current release stays first, so it is the first one verified.
        assert_eq!(
            staged_i02.repos(),
            [I02_CURRENT_RELEASE.repo, I02_TEST_STAGE.repo]
        );
        assert!(approves(&staged_i02, &I02_TEST_STAGE));
        assert!(approves(&staged_i02, &I02_CURRENT_RELEASE));
        assert!(!approves(&current_i02, &I02_TEST_STAGE));
        assert!(!approves(&i01, &I02_TEST_STAGE));
    }

    #[test]
    fn stock_i02_approves_only_its_new_release() {
        let policy = policy_for(endpoint(MapleInstanceId::I02));
        assert_eq!(policy.approved.len(), 1);
        let release = &policy.approved[0];
        assert_eq!(release.repo, "MaplePrivacyLabs/inference-deepseek");
        assert_eq!(release.tag, "v0.1.2");
        assert_eq!(
            release.digest,
            "4bc5f2119cb55e125c9cc5c57e3fe120df98f8145dc675c8d201ac01882989ec"
        );
        assert_eq!(
            release.snp_launch.to_string(),
            "3f35fb1e92c599564e85e2b9adae8778bf8a9f553a8c936132301f09d4aa31a4a505b1c8f96cef407c53f4df67d23030"
        );
        assert_eq!(policy.repos(), ["MaplePrivacyLabs/inference-deepseek"]);
        assert!(!approves(&policy, &I01_CURRENT_RELEASE));
        assert!(direct_policy(MapleInstanceId::I02).unwrap().is_some());
        assert!(direct_policy(MapleInstanceId::I01).unwrap().is_some());
    }

    #[test]
    fn uncensored_i01_approves_only_its_new_release() {
        let policy = policy_for(endpoint(MapleInstanceId::I01));
        assert_eq!(policy.approved.len(), 1);
        let release = &policy.approved[0];
        assert_eq!(
            release.repo,
            "MaplePrivacyLabs/inference-deepseek-uncensored"
        );
        assert_eq!(release.tag, "v0.1.0+b300");
        assert_eq!(
            release.digest,
            "6a8e906bc31caaccf5b6bbe84dec99c7213ec1d77aa360df464d531744590ac2"
        );
        assert_eq!(
            release.snp_launch.to_string(),
            "9e01436661dff4702b622a51a7bbcb83ce0a61d0c2bdc2de5a2492fc1dec70bb1997df829147a154e11fdfc6dbe044fe"
        );
        assert_eq!(
            policy.repos(),
            ["MaplePrivacyLabs/inference-deepseek-uncensored"]
        );
        assert!(!approves(&policy, &I02_CURRENT_RELEASE));
    }

    #[test]
    fn retired_releases_are_not_approved_on_either_guest() {
        for instance in MapleInstanceId::ALL {
            assert!(endpoint(instance).staged_releases.is_empty());
            for approved in policy_for(endpoint(instance)).approved {
                assert_ne!(approved.tag, "v0.1.0");
                for retired_digest in [
                    "7f421e8aefdd63181b71b9d202ac6a3b05b976c6ecb156354a20ff691931f5be",
                    "1d92cd06f7b47179bf81612f6a41fdb0a329253affef1803f0cdeb5175cc80c4",
                    "2d69e8544e5b2706d0e6004bc0ac925dc9abd79cb284f9400c26c9d145e25b00",
                ] {
                    assert_ne!(approved.digest, retired_digest);
                }
                for retired_launch in [
                    "f645aa2a679cf7ed575bb5d9d2fa18104d24ae545950d6c3669b6af1d81328f099f3437f9129c608ac9ff5a97927d365",
                    "b391ea9621062080c01c3686bb70e8889148f6d6c7ca71eec6c149f4132b50f7b47e251f113a9a0861f05e546203aa39",
                    "d4d5fd1166413a3229a83899b99c06fb8cb904badb2ed5b9b8e6cb7414ad110628b6780eddaf10c42cbc996909145541",
                ] {
                    assert_ne!(approved.snp_launch.to_string(), retired_launch);
                }
            }
        }
    }
}
