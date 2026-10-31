"""Create immutable release facts; dispatch and stage approvals stay separate."""
from __future__ import annotations

from contextlib import redirect_stdout
from copy import deepcopy
from datetime import datetime
import io
import json
import os
from pathlib import Path
import re
import stat
import subprocess
from urllib.parse import urlsplit

import publisher as canonical

from .context import (
    CODE_PATHS, DESKTOP_WORKFLOW, PLATFORMS, ALL_PLATFORMS, REPOSITORY, SCHEMA_BINDINGS,
    SCHEMA_BUNDLE, SCHEMA_FREEZE, SCHEMA_PUBLIC, SCHEMA_R2, TARGETS, WORKFLOW, Policy, decode, digest,
    need, positive, read_regular, reference, sha, source, tooling_digest, selected_platforms,
    manifest_names, envelope_names, pointer_names, predecessor_platforms, platform_counts, r2_request_count,
)

PREVIOUS_SCHEMA = "agent-release-previous-v1"
NAMES = ("manifest.json", "linux-manifest.json", "windows-manifest.json")
ENVELOPES = ("envelope.json", "linux-envelope.json", "windows-envelope.json")
PREVIOUS_REFS = ("previous_private_r2", "previous_root_acceptance", "previous_public", "research_preservation")
MAX_API = 8 * 1024**2


def _state(path):
    root = Path(path)
    need(root.is_absolute() and root.parent.resolve(strict=True) == root.parent,
         "state path must be absolute and canonical")
    if not root.exists() and not root.is_symlink():
        root.mkdir(mode=0o700)
    info = root.lstat()
    need(root.resolve(strict=True) == root and stat.S_ISDIR(info.st_mode)
         and stat.S_IMODE(info.st_mode) == 0o700 and info.st_uid == os.getuid(),
         "state directory must be owned, canonical and private0700")
    return root


def _output(root, path):
    path = Path(path)
    need(path.is_absolute() and path.parent.resolve(strict=True) == path.parent
         and path.is_relative_to(root), "output must be inside owned state")
    need(not path.exists() and not path.is_symlink(), "output already exists; preserve the previous snapshot")
    return path


def _write(root, path, raw):
    path = _output(root, path)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "wb") as handle:
        handle.write(raw)
        handle.flush()
        os.fsync(handle.fileno())
    fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)
    return {"path": str(path), "sha256": sha(raw)}


def _emit(root, path, value):
    return _write(root, path, canonical.encode_json(value))


def _pinned(path, expected):
    reference({"path": str(path), "sha256": expected})
    raw = read_regular(Path(path))
    need(sha(raw) == expected, "pinned input changed")
    return raw


def _get(policy, path):
    # Every caller builds a fixed same-repository endpoint from validated IDs.
    result = subprocess.run(
        [policy.tool("gh"), "api", "--hostname", "github.com", "--method", "GET",
         f"repos/{REPOSITORY}/{path}"], stdin=subprocess.DEVNULL,
        capture_output=True, timeout=60)
    need(result.returncode == 0 and len(result.stdout) <= MAX_API, "bounded GitHub GET failed")
    return decode(result.stdout)


def _previous(path, expected, version):
    raw = _pinned(path, expected)
    value = decode(raw)
    keys = {"schema", "previous", "previous_signed_envelopes", *PREVIOUS_REFS}
    need(isinstance(value, dict) and set(value) == keys and value["schema"] == PREVIOUS_SCHEMA,
         "invalid predecessor descriptor")
    old = value["previous"]
    need(isinstance(old, dict) and set(old) == {"version", "build", "source_sha", "publisher_run", "publisher_attempt"},
         "invalid predecessor identity")
    need(isinstance(old["version"], str) and canonical.version_tuple(old["version"]) < canonical.version_tuple(version)
         and isinstance(old["build"], str) and re.fullmatch(r"[1-9][0-9]{0,17}", old["build"])
         and source(old["source_sha"]) and positive(old["publisher_run"]) and positive(old["publisher_attempt"]),
         "invalid or nonadvancing predecessor")
    predecessor_platforms(value)
    for item in [value[key] for key in PREVIOUS_REFS] + list(value["previous_signed_envelopes"].values()):
        reference(item)
        _pinned(item["path"], item["sha256"])
    return {key: deepcopy(value[key]) for key in keys - {"schema"}}


def plan(args):
    """Qualify one exact producer and write a new, unselected binding snapshot."""
    need(args.network is True, "planning GitHub GETs require explicit --network")
    need(positive(args.producer_run), "invalid producer run")
    canonical.version_tuple(args.version)
    root = _state(args.state_dir)
    output = _output(root, args.output)
    policy = Policy(args.policy, args.policy_sha256)
    previous = _previous(args.previous_manifest, args.previous_manifest_sha256, args.version)
    notes = read_regular(Path(args.notes_file), canonical.MAX_NOTES)
    notes.decode("utf-8")
    key = read_regular(Path(args.public_key), canonical.MAX_SIGNATURE)
    primary = canonical.public_key_text(args.public_key)
    repo = Path(__file__).resolve().parents[4]
    profiles = decode(read_regular(repo / "apps/maple-agent/release-profiles.json"))
    need(primary == profiles["prod"]["update_public_keys"][0], "release key must be the compiled primary")
    platforms = ALL_PLATFORMS
    canonical.validate_pins(primary, profiles, profiles, linux=True, windows=True, windows_arm64=True)
    run = _get(policy, f"actions/runs/{args.producer_run}")
    jobs = _get(policy, f"actions/runs/{args.producer_run}/jobs?filter=latest&per_page=100")
    canonical.validate_run(run, jobs, linux=True, windows=True, windows_arm64=True)
    need(run["id"] == args.producer_run and positive(run["run_attempt"]), "selected producer run differs")
    need(isinstance(run.get("created_at"), str)
         and re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", run["created_at"]),
         "invalid producer publication date")
    datetime.fromisoformat(run["created_at"].replace("Z", "+00:00"))
    inventory = _get(policy, f"actions/runs/{args.producer_run}/artifacts?per_page=100")
    rows = inventory.get("artifacts", [])
    need(isinstance(rows, list) and type(inventory.get("total_count")) is int
         and inventory["total_count"] == len(rows), "incomplete artifact inventory")
    selected, artifacts = [], {}
    for platform in platforms:
        name = f"maple-agent-prod-{TARGETS[platform]}-{args.producer_run}"
        matches = [item for item in rows if item.get("name") == name]
        need(len(matches) == 1, "selected final artifact missing or duplicated")
        item = matches[0]
        run_ref = item.get("workflow_run", {})
        need(positive(item.get("id")) and type(item.get("size_in_bytes")) is int
             and 0 < item["size_in_bytes"] <= 512 * 1024**2 and item.get("expired") is False
             and isinstance(item.get("digest"), str) and item["digest"].startswith("sha256:")
             and digest(item["digest"][7:]) and type(run_ref.get("id")) is int
             and run_ref["id"] == run["id"] and run_ref.get("head_sha") == run["head_sha"]
             and run_ref.get("head_branch") == "master", "selected final artifact identity differs")
        selected.append(item)
        artifacts[platform] = {"id": item["id"], "name": name, "size": item["size_in_bytes"],
                               "zip_sha256": item["digest"][7:]}
    need(len({item["id"] for item in selected}) == len(platforms), "selected artifact IDs are ambiguous")
    commit = _get(policy, f"git/commits/{run['head_sha']}")
    need(commit.get("sha") == run["head_sha"] and source(commit.get("tree", {}).get("sha")),
         "producer source tree differs")
    master = _get(policy, "git/ref/heads/master").get("object", {})
    need(master.get("type") == "commit" and source(master.get("sha")), "invalid publisher master source")
    identity = {"run_id": run["id"], "run_attempt": run["run_attempt"], "source_sha": run["head_sha"],
                "version": args.version, "build": str(run["run_number"]), "pub_date": run["created_at"]}
    qualification = {
        "schema": "agent-release-producer-qualification-v1", "run_id": run["id"], "attempt": run["run_attempt"],
        "source": run["head_sha"], "version": args.version, "build": run["run_number"],
        "producer_terminal": True, "required_included_jobs_all_success": True,
        "scope": {"include_linux": True, "include_windows": True, "include_windows_arm64": True}, "run": run,
        "jobs": jobs, "selected_final_artifact_metadata": selected,
    }
    bindings = {
        "schema": SCHEMA_BINDINGS, "policy_sha256": args.policy_sha256,
        "tooling_sha256": tooling_digest(repo), "repo": str(repo), "platforms": list(platforms),
        "code_pins": {name: sha(read_regular(repo / name)) for name in CODE_PATHS},
        "producer_identity": identity, "payload_tree": commit["tree"]["sha"], "artifacts": artifacts,
        "publisher_source": master["sha"], "freeze": None, "prepared_files": {}, "prepared_bundle": None,
        "publication_identity": None, "selected_publication": None, **previous,
    }
    # These writes are exclusive and durable. A partial plan stays available for inspection.
    bindings["qualification"] = _emit(root, root / "qualification.json", qualification)
    bindings["notes"] = _write(root, root / "release-notes.txt", notes)
    bindings["public_key"] = _write(root, root / "public-key.pub", key)["path"]
    result = _emit(root, output, bindings)
    return {"bindings": result["path"], "bindings_sha256": result["sha256"],
            "qualification": bindings["qualification"]}


def _snapshot(ctx, output, changes):
    value = deepcopy(ctx.bindings)
    value.update(changes)
    result = _emit(ctx.root, output, value)
    return {"bindings": result["path"], "bindings_sha256": result["sha256"]}


def _frozen(ctx):
    platforms = selected_platforms(ctx.bindings['platforms'])
    frozen = decode(ctx.pinned(ctx.bindings["freeze"]))
    producer = {**ctx.identity, "artifacts": {platform: item["name"]
                                             for platform, item in ctx.bindings["artifacts"].items()}}
    need(frozen.get("schema") == SCHEMA_FREEZE and frozen.get("canonical_prepare_completed") is True
         and frozen.get("source") == ctx.source and frozen.get("tree") == ctx.tree
         and frozen.get("platforms") == list(platforms) and frozen.get("producer") == producer
         and frozen.get("files") == ctx.bindings["prepared_files"]
         and len(frozen["files"]) == platform_counts(platforms)['prepared']
         and frozen.get("notes") == ctx.bindings["notes"], "frozen release identity differs")
    return frozen


def freeze(ctx, args):
    """Run the canonical preparer, including its default final-file attestations."""
    need(args.network is True, "canonical preparation requires explicit --network")
    need(ctx.bindings.get("freeze") is None and not ctx.bindings.get("publication_identity"),
         "release facts already frozen or selected")
    _output(ctx.root, args.output)
    for name in ("qualification", "notes"):
        ctx.pinned(ctx.bindings[name])
    prepared = Path(getattr(args, "prepared_bundle", None) or ctx.root / "local-prepared")
    _output(ctx.root, prepared)
    platforms = selected_platforms(ctx.bindings['platforms'])
    names = manifest_names(platforms)
    directories = {platform: ctx.root / f"packages-{platform}-{ctx.bindings['artifacts'][platform]['id']}"
                   for platform in platforms}
    for path in directories.values():
        need(path.is_absolute() and path.resolve(strict=True) == path and path.is_dir() and not path.is_symlink(),
             "missing or unsafe exact producer package directory")
    tool = Path(ctx.tool("gh"))
    need(tool.name == "gh", "canonical preparer requires the pinned gh executable")
    previous_path = os.environ.get("PATH")
    previous_mask = os.umask(0o077)
    log = io.StringIO()
    try:
        os.environ["PATH"] = str(tool.parent)
        # Do not inject an attestor or replace any canonical preparation function.
        with redirect_stdout(log):
            ctx.core.prepare(str(ctx.producer_run), ctx.version, ctx.build, directories["macos"], directories["linux"],
                             Path(ctx.bindings["notes"]["path"]), prepared, Path(ctx.bindings["public_key"]),
                             pub_date=ctx.identity["pub_date"], windows=directories["windows"],
                             **({'windows_arm64': directories['windows-arm64']} if 'windows-arm64' in platforms else {}))
    finally:
        os.umask(previous_mask)
        if previous_path is None:
            os.environ.pop("PATH", None)
        else:
            os.environ["PATH"] = previous_path
    trust = decode(read_regular(prepared / "trust.json"))
    producer = {key: ctx.identity[key] for key in ("run_id", "run_attempt", "source_sha", "build", "pub_date")}
    producer["artifacts"] = {platform: item["name"] for platform, item in ctx.bindings["artifacts"].items()}
    need(trust == producer, "canonical preparation selected a different producer")
    publications = ctx.core.bundle_manifests(prepared)
    need(tuple(item["name"] for item in publications) == names, "canonical preparation omitted a platform")
    main = publications[0]["manifest"]
    need(all(main.get(key) == ctx.identity[key] for key in ("version", "build", "source_sha", "pub_date"))
         and main.get("notes") == ctx.pinned(ctx.bindings["notes"]).decode("utf-8"),
         "canonical prepared release differs")
    payloads = ctx.core.bundle_records(prepared, main)
    _, metadata = ctx.core.bundle_metadata(prepared, main)
    expected = set(names) | {"trust.json", "public-key.pub", "metadata-inventory.json"}
    expected |= {str(path.relative_to(prepared)) for path, _ in payloads + metadata}
    files = {}
    for path in prepared.rglob("*"):
        relative = str(path.relative_to(prepared))
        need(not path.is_symlink(), "symlink in canonical prepared bundle")
        if path.is_dir():
            need(relative in {"payloads", "metadata"}, "unexpected prepared directory")
        else:
            hashed, size = ctx.core.hash_file(path, 512 * 1024**2 if relative.startswith("payloads/") else 4 * 1024**2)
            files[relative] = {"sha256": hashed, "size": size}
    need(set(files) == expected and len(files) == platform_counts(platforms)['prepared'], "canonical prepared file inventory differs")
    need(read_regular(prepared / "metadata/provenance.json") == ctx.core.durable_provenance(trust),
         "canonical provenance differs")
    log_bytes = log.getvalue().encode()
    need(len(log_bytes) <= 1024**2, "canonical preparation output exceeds bound")
    log_ref = _write(ctx.root, ctx.root / "canonical-prepare.log", log_bytes)
    execution = _emit(ctx.root, ctx.root / "canonical-prepare.json", {
        "schema": "agent-release-canonical-prepare-v1", "completed": True,
        "core_sha256": ctx.core_sha256, "tooling_sha256": ctx.tooling_sha256,
        "producer": deepcopy(ctx.identity), "default_attestation_file_count": 16 + 5 * ('windows-arm64' in platforms),
        "log": log_ref, "prepared_bundle": str(prepared),
    })
    frozen = {"schema": SCHEMA_FREEZE, "canonical_prepare_completed": True, "source": ctx.source,
              "tree": ctx.tree, "platforms": list(platforms), "producer": producer,
              "files": files, "notes": deepcopy(ctx.bindings["notes"]), "canonical_execution": execution}
    frozen["producer"]["version"] = ctx.version
    frozen_ref = _emit(ctx.root, ctx.root / "prepared-freeze.json", frozen)
    result = _snapshot(ctx, args.output, {"freeze": frozen_ref, "prepared_files": files,
                                        "prepared_bundle": str(prepared), "public_key": str(prepared / "public-key.pub")})
    result.update({"freeze": frozen_ref, "prepared_bundle": str(prepared), "canonical_execution": execution})
    return result


def bind_publication(ctx, args):
    """Select an actual publication once; this command never dispatches it."""
    _output(ctx.root, args.output)
    need(ctx.bindings.get("freeze") is not None and ctx.bindings.get("publication_identity") is None,
         "a frozen, unselected release is required")
    _frozen(ctx)
    platforms = selected_platforms(ctx.bindings['platforms'])
    selected = decode(_pinned(args.selection, args.selection_sha256))
    publication = {key: selected.get(key) for key in ("run_id", "run_attempt", "source_sha", "workflow")}
    need(positive(publication["run_id"]) and positive(publication["run_attempt"])
         and publication["source_sha"] == ctx.bindings["publisher_source"] and publication["workflow"] == WORKFLOW,
         "selected actual publication identity differs")
    need(type(selected.get("producer_run")) is int and selected["producer_run"] == ctx.producer_run
         and type(selected.get("producer_attempt")) is int and selected["producer_attempt"] == ctx.producer_attempt
         and selected.get("payload_source") == ctx.source and selected.get("version") == ctx.version
         and selected.get("platforms") == list(platforms), "selected publication payload differs")
    inputs = {"build_run_id": str(ctx.producer_run), "version": ctx.version,
              "notes": ctx.pinned(ctx.bindings["notes"]).decode("utf-8"), "include_linux": True, "include_windows": True}
    arm_inputs = {**inputs, "include_windows_arm64": 'windows-arm64' in platforms}
    actual_inputs = selected.get('inputs')
    need(isinstance(actual_inputs, dict)
         and all(type(actual_inputs.get(key)) is bool for key in ('include_linux', 'include_windows'))
         and ('include_windows_arm64' not in actual_inputs or type(actual_inputs['include_windows_arm64']) is bool)
         and (actual_inputs == arm_inputs or (platforms == PLATFORMS and actual_inputs == inputs)),
         "selected publication inputs differ")
    return _snapshot(ctx, args.output, {"publication_identity": publication,
                                       "selected_publication": {"path": str(args.selection), "sha256": args.selection_sha256}})


def bind_public(ctx, args):
    """Bind successful-run and signed-proof facts for separate public verification."""
    _output(ctx.root, args.output)
    need(not ctx.bindings.get("successful_publication_run"), "public facts already bound")
    platforms = selected_platforms(ctx.bindings['platforms'])
    envelopes_expected = envelope_names(platforms)
    identity = ctx.publication
    success = decode(_pinned(args.successful_run, args.successful_run_sha256))
    need(type(success.get("id")) is int and success["id"] == identity["run_id"]
         and type(success.get("run_attempt")) is int and success["run_attempt"] == identity["run_attempt"]
         and success.get("head_sha") == identity["source_sha"] and success.get("head_branch") == "master"
         and success.get("path") == WORKFLOW and success.get("event") == "workflow_dispatch"
         and success.get("repository", {}).get("full_name") == REPOSITORY
         and success.get("head_repository", {}).get("full_name") == REPOSITORY
         and success.get("status") == "completed" and success.get("conclusion") == "success",
         "actual publication has not successfully completed")
    proof = decode(_pinned(args.signed_stage_proof, args.signed_stage_proof_sha256))
    need(proof.get("schema") == SCHEMA_BUNDLE and proof.get("stage") == "signed"
         and proof.get("producer") == ctx.identity and proof.get("publication") == identity
         and proof.get("bindings_sha256") == ctx.bindings_sha256
         and proof.get("tooling_sha256") == ctx.tooling_sha256 and proof.get("policy_sha256") == ctx.policy_sha256
         and proof.get("core_sha256") == ctx.core_sha256 and proof.get("primary_key_sha256") == ctx.primary_sha256
         and proof.get("verified_primary_signatures") == sorted(envelopes_expected),
         "signed-stage proof identity differs")
    bundle = Path(args.signed_bundle)
    need(bundle.is_absolute() and bundle.resolve(strict=True) == bundle and bundle.is_dir()
         and not bundle.is_symlink() and bundle.is_relative_to(ctx.root), "unsafe signed bundle")
    frozen = _frozen(ctx)
    files = proof.get("files", {})
    need(set(files) == set(ctx.bindings["prepared_files"]) | {"reservation.json", *envelopes_expected}
         and len(files) == platform_counts(platforms)['signed']
         and frozen["files"] == ctx.bindings["prepared_files"], "signed proof inventory differs")
    actual = set()
    for path in bundle.rglob("*"):
        need(not path.is_symlink() and path.resolve(strict=True) == path, "unsafe signed bundle member")
        if path.is_dir():
            need(str(path.relative_to(bundle)) in {"payloads", "metadata"}, "unexpected signed bundle directory")
        else:
            actual.add(str(path.relative_to(bundle)))
    need(actual == set(files), "signed bundle file inventory differs")
    for name, record in files.items():
        need(isinstance(record, dict) and set(record) == {"size", "sha256"}
             and type(record["size"]) is int and record["size"] > 0 and digest(record["sha256"]),
             "invalid signed file record")
        hashed, size = ctx.core.hash_file(bundle / name)
        need(record == {"sha256": hashed, "size": size}, "signed bundle differs from proof")
        if name in ctx.bindings["prepared_files"]:
            need(record == ctx.bindings["prepared_files"][name], "frozen prepared bytes changed")
    envelopes = {platform: files[name]["sha256"] for platform, name in zip(platforms, envelopes_expected)}
    return _snapshot(ctx, args.output, {
        "successful_publication_run": {"path": str(args.successful_run), "sha256": args.successful_run_sha256},
        "signed_stage_proof": {"path": str(args.signed_stage_proof), "sha256": args.signed_stage_proof_sha256},
        "signed_bundle": str(bundle), "envelope_sha256": envelopes,
        "operation_bindings": {"path": str(ctx.bindings_path), "sha256": ctx.bindings_sha256},
    })


def accept(ctx, args):
    """Record verified release evidence as the next release's pinned baseline."""
    from .public import PublicVerifier

    output = _output(ctx.root, args.output)
    acceptance_path = _output(ctx.root, ctx.root / "root-acceptance.json")
    research_path = _output(ctx.root, ctx.root / "research-preservation.json")
    need(len({output, acceptance_path, research_path}) == 3, "baseline and evidence outputs must differ")
    bindings = ctx.bindings
    platforms = selected_platforms(bindings['platforms'])
    names, envelopes = manifest_names(platforms), envelope_names(platforms)
    public_inventory = PublicVerifier.inventory(bindings)
    publication = ctx.publication
    public = decode(_pinned(args.public_receipt, args.public_receipt_sha256))
    private = decode(_pinned(args.private_receipt, args.private_receipt_sha256))
    signed = decode(ctx.pinned(bindings["signed_stage_proof"]))
    operation = decode(ctx.pinned(bindings["operation_bindings"]))
    operation_sha = bindings["operation_bindings"]["sha256"]
    need(signed.get("schema") == SCHEMA_BUNDLE and signed.get("stage") == "signed"
         and signed.get("bindings_sha256") == operation_sha and signed.get("producer") == ctx.identity
         and signed.get("publication") == publication and signed.get("verified_primary_signatures") == sorted(envelopes)
         and signed.get("primary_key_sha256") == ctx.primary_sha256
         and all(signed.get(key) == value for key, value in {
             "tooling_sha256": ctx.tooling_sha256, "policy_sha256": ctx.policy_sha256,
             "core_sha256": ctx.core_sha256}.items()), "accepted signed proof differs")
    need(all(operation.get(key) == bindings[key] for key in (
        "producer_identity", "publication_identity", "payload_tree", "platforms", "artifacts", "freeze", "prepared_files",
        "prepared_bundle", "notes", "public_key", "policy_sha256", "tooling_sha256", "code_pins")),
        "public and operator snapshots differ")
    need(public.get("schema") == SCHEMA_PUBLIC and public.get("all_checks_passed") is True
         and public.get("agent_checks_passed") is True and public.get("research_checks_passed") is True
         and public.get("source") == ctx.source and public.get("version") == ctx.version and public.get("build") == ctx.build
         and type(public.get("producer_run")) is int and public["producer_run"] == ctx.producer_run
         and type(public.get("producer_attempt")) is int and public["producer_attempt"] == ctx.producer_attempt
         and public.get("publication") == publication and public.get("bindings_sha256") == ctx.bindings_sha256
         and public.get("signed_stage_proof") == bindings["signed_stage_proof"]
         and public.get("freeze_sha256") == bindings["freeze"]["sha256"]
         and public.get("envelope_sha256") == bindings["envelope_sha256"]
         and public.get("previous") == bindings["previous"]
         and public.get("previous_public_sha256") == bindings["previous_public"]["sha256"],
         "successful current public evidence required")
    for record in (public, private):
        need(record.get("policy_sha256") == ctx.policy_sha256 and record.get("tooling_sha256") == ctx.tooling_sha256,
             "release evidence policy or tooling differs")
    need(public.get("core_sha256") == ctx.core_sha256
         and public.get("full_payload_streams") == public_inventory['full_streams']
         and public.get("successful_full_payload_streams") == public_inventory['full_streams']
         and public.get("fixed_request_count") == public_inventory['fixed_requests']
         and type(public.get("total_request_count")) is int
         and public_inventory['fixed_requests'] <= public["total_request_count"] <= public_inventory['max_requests']
         and public.get("included_platforms") == list(platforms)
         and public.get("preserved_previous_immutables") == list(predecessor_platforms(bindings))
         and public.get("duplicate_full_payload_files_saved") == 0
         and public.get("credentials_used") is False and public.get("redirects_followed") is False
         and public.get("producer_attestations_verified_here") is False
         and public.get("analytics_probe_header") == {"X-Maple-Download-Probe": "1"}
         and public.get("research", {}).get("state") in {"unchanged", "independent_release_verified"},
         "public verification inventory differs")
    requests = public.get("requests", [])
    need(isinstance(requests, list) and len(requests) == public_inventory['fixed_requests']
         and all(item.get("check_passed") is True for item in requests), "public request checks incomplete")
    streams = [item for item in requests if item.get("kind") == "payload-full"]
    need(len(streams) == public_inventory['full_streams']
         and {urlsplit(item.get("url", "")).hostname for item in streams} == set(PublicVerifier.HOSTS)
         and len({item.get("url") for item in streams}) == public_inventory['full_streams'],
         "both-host full payload evidence incomplete")
    stream_paths = []
    for host in PublicVerifier.HOSTS:
        urls = [urlsplit(item["url"]) for item in streams if urlsplit(item["url"]).hostname == host]
        paths = {url.path for url in urls}
        need(len(urls) == len(paths) == public_inventory['full_streams'] // len(PublicVerifier.HOSTS)
             and all(url.scheme == "https" and url.netloc == host and not url.query and not url.fragment for url in urls),
             "per-host full payload evidence differs")
        stream_paths.append(paths)
    need(all(paths == stream_paths[0] for paths in stream_paths), "public hosts verified different payloads")
    need(private.get("schema") == SCHEMA_R2 and private.get("phase") == "after-publish"
         and private.get("passed") is True and private.get("request_count") == r2_request_count(bindings)
         and private.get("bindings_sha256") == operation_sha
         and private.get("route_sha256") == ctx.route_sha256 and digest(private.get("previous_proof_sha256")),
         "successful operator-bound after-publication private proof required")
    start, end = (datetime.fromisoformat(private[name]) for name in ("started_at", "completed_at"))
    need(start.tzinfo is not None and end.tzinfo is not None and 0 <= (end - start).total_seconds() <= 181,
         "invalid private proof time interval")
    baseline = decode(ctx.pinned(bindings["previous_private_r2"]))["objects"]
    prefix = f"stable/releases/{ctx.version}/"
    pointers = pointer_names(platforms)
    reservations = {prefix + "publication-" + name: name for name in names}
    reservations[prefix + "publication-metadata.json"] = "metadata-inventory.json"
    old_keys = set(bindings["previous_signed_envelopes"])
    objects = private.get("objects", {})
    expected_keys = set(pointers) | old_keys | set(reservations) | {prefix + name for name in names}
    need(isinstance(objects, dict) and set(objects) == expected_keys and len(objects) == r2_request_count(bindings),
         "private metadata object inventory differs")
    for key, value in objects.items():
        need(isinstance(value, dict) and set(value) == {"sha256", "size", "etag"}
             and digest(value["sha256"]) and type(value["size"]) is int and value["size"] > 0
             and isinstance(value["etag"], str) and value["etag"]
             and all(32 <= ord(char) != 127 for char in value["etag"]), "invalid private object proof")
        if key in old_keys:
            need(value == baseline[key], "accepted previous immutable bytes or ETag changed")
    envelope_refs = {}
    for platform, name, envelope, pointer in zip(platforms, names, envelopes, pointers):
        path = Path(bindings["signed_bundle"]) / envelope
        raw = read_regular(path, ctx.core.MAX_ENVELOPE)
        expected = {"sha256": sha(raw), "size": len(raw)}
        need(expected["sha256"] == bindings["envelope_sha256"][platform]
             and signed.get("files", {}).get(envelope) == expected,
             "accepted signed envelope bytes changed")
        for key in (pointer, prefix + name):
            need(all(objects[key][field] == value for field, value in expected.items()),
                 "private latest or immutable envelope differs")
        envelope_refs[prefix + name] = {"path": str(path), "sha256": expected["sha256"]}
    frozen = _frozen(ctx)
    for key, name in reservations.items():
        expected = frozen["files"][name]
        need(all(objects[key][field] == value for field, value in expected.items()),
             "private reserved publication bytes changed")
    # Carry forward the accepted current Research state. An independently
    # qualified Research release may have advanced since the old preservation
    # record; pairing that old record with the new public baseline would make
    # the next release's unchanged-state guard reject its own predecessor.
    ctx.pinned(bindings["research_preservation"])
    latest = [item for item in requests if item.get("kind") == "research-latest"
              and item.get("method") == "GET" and item.get("url") == "https://updates.trymaple.ai/latest.json"]
    need(len(latest) == 1 and latest[0].get("status") == 200
         and digest(latest[0].get("body_sha256"))
         and latest[0]["body_sha256"] == public["research"].get("metadata_sha256")
         and latest[0].get("research_version") == public["research"].get("version"),
         "accepted Research metadata evidence differs")
    research_requests = [deepcopy(latest[0])]
    for key in PublicVerifier.RESEARCH_KEYS:
        matches = [item for item in requests if item.get("kind") == "research-alias" and item.get("key") == key
                   and item.get("method") == "GET"
                   and item.get("url") == "https://updates.trymaple.ai/download/research/stable/" + key]
        need(len(matches) == 1 and matches[0].get("status") == 302
             and isinstance(matches[0].get("headers", {}).get("location"), str),
             "accepted Research alias evidence differs")
        research_requests.append(deepcopy(matches[0]))
    need(len(research_requests) == 7, "Research preservation GET inventory differs")
    research = _emit(ctx.root, research_path, {
        "schema": "agent-release-research-preservation-v1", "passed": True, "request_count": 7, "writes": 0,
        "public_receipt": {"path": str(args.public_receipt), "sha256": args.public_receipt_sha256},
        "research": deepcopy(public["research"]), "requests": research_requests,
    })
    acceptance = _emit(ctx.root, acceptance_path, {
        "schema": "agent-release-root-acceptance-v1", "release_published": True,
        "version": ctx.version, "build": ctx.build, "source": ctx.source,
        "producer_run": ctx.producer_run, "producer_attempt": ctx.producer_attempt,
        "publisher_run": publication["run_id"], "publisher_attempt": publication["run_attempt"],
        "envelopes": deepcopy(bindings["envelope_sha256"]), "public_receipt_sha256": args.public_receipt_sha256,
        "private_receipt_sha256": args.private_receipt_sha256, "bindings_sha256": ctx.bindings_sha256,
        "operation_bindings_sha256": operation_sha, "tooling_sha256": ctx.tooling_sha256,
        "policy_sha256": ctx.policy_sha256, "research_preservation": research,
    })
    descriptor = {"schema": PREVIOUS_SCHEMA, "previous": {
        "version": ctx.version, "build": ctx.build, "source_sha": ctx.source,
        "publisher_run": publication["run_id"], "publisher_attempt": publication["run_attempt"]},
        "previous_private_r2": {"path": str(args.private_receipt), "sha256": args.private_receipt_sha256},
        "previous_public": {"path": str(args.public_receipt), "sha256": args.public_receipt_sha256},
        "previous_root_acceptance": acceptance, "previous_signed_envelopes": envelope_refs,
        "research_preservation": research}
    result = _emit(ctx.root, output, descriptor)
    return {"baseline_manifest": result["path"], "baseline_manifest_sha256": result["sha256"],
            "root_acceptance": acceptance, "research_preservation": research}


def add_arguments(parser, operation):
    """Arguments specific to one data-lifecycle command; common context is CLI-owned."""
    if operation == "plan":
        parser.add_argument("--producer-run", type=int, required=True)
        parser.add_argument("--version", required=True)
        for name in ("notes-file", "public-key", "previous-manifest"):
            parser.add_argument("--" + name, type=Path, required=True)
        parser.add_argument("--previous-manifest-sha256", required=True)
    elif operation == "freeze":
        parser.add_argument("--prepared-bundle", type=Path)
    elif operation == "bind-publication":
        parser.add_argument("--selection", type=Path, required=True)
        parser.add_argument("--selection-sha256", required=True)
    elif operation == "bind-public":
        for name in ("successful-run", "signed-stage-proof"):
            parser.add_argument("--" + name, type=Path, required=True)
            parser.add_argument("--" + name + "-sha256", required=True)
        parser.add_argument("--signed-bundle", type=Path, required=True)
    elif operation == "accept":
        for name in ("public-receipt", "private-receipt"):
            parser.add_argument("--" + name, type=Path, required=True)
            parser.add_argument("--" + name + "-sha256", required=True)
    else:
        need(False, "unknown planning operation")
    parser.add_argument("--output", type=Path, required=True)
