#!/usr/bin/env python3
"""Reusable operator CLI. Release facts change; executable helpers do not."""
import argparse
import json
from pathlib import Path
import sys

# Isolated Python deliberately omits the script directory from sys.path.
# Load only this trusted checkout's fixed sibling package, never the caller's cwd.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from release_helpers.context import Context, OperatorError, Policy


def common(parser, bindings=True):
    parser.add_argument("--state-dir", type=Path, required=True)
    parser.add_argument("--policy", type=Path, required=True)
    parser.add_argument("--policy-sha256", required=True)
    if bindings:
        parser.add_argument("--bindings", type=Path, required=True)
        parser.add_argument("--bindings-sha256", required=True)


def main(argv=None):
    from release_helpers import downloads, dispatch, planning
    from release_helpers.approval import Approval
    from release_helpers.public import PublicVerifier
    from release_helpers.bundle import BundleVerifier, PHASES
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    p = commands.add_parser("check-policy")
    p.add_argument("--policy", type=Path, required=True)
    p.add_argument("--policy-sha256", required=True)
    for name in ("plan", "freeze", "bind-publication", "bind-public", "accept"):
        p = commands.add_parser(name)
        common(p, bindings=name != "plan")
        planning.add_arguments(p, name)
        if name in ("plan", "freeze"):
            p.add_argument("--network", action="store_true")
    for name in ("download-producer", "download-stage"):
        p = commands.add_parser(name)
        common(p)
        downloads.add_arguments(p, name)
    for name in ("dispatch", "reconcile-dispatch"):
        p = commands.add_parser(name)
        common(p)
        dispatch.add_arguments(p, name)
    p = commands.add_parser("approve")
    common(p)
    Approval.add_arguments(p)
    p = commands.add_parser("verify-public")
    common(p)
    PublicVerifier.add_arguments(p)
    p = commands.add_parser("verify-bundle")
    common(p)
    p.add_argument("--bundle", type=Path, required=True)
    p.add_argument("--stage", choices=("prepared", "reserved", "signed"), required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--previous-proof", type=Path)
    p.add_argument("--previous-sha256")
    p.add_argument("--r2-phase", choices=PHASES)
    p.add_argument("--r2-output", type=Path)
    p.add_argument("--prior-r2", type=Path)
    p.add_argument("--prior-r2-sha256")
    p.add_argument("--network", action="store_true")
    args = parser.parse_args(argv)
    if args.command == "check-policy":
        policy = Policy(args.policy, args.policy_sha256)
        route = policy.verify_route()
        for role in ("gh", "minisign"):
            policy.tool(role)
        result = {"local_policy_verified": True, "policy_sha256": args.policy_sha256,
                  "route_sha256": route["sha256"], "network_requests": 0,
                  "scope": "local schema, route and tool bytes; live environment policy is checked at each approval"}
    elif args.command == "plan":
        result = planning.plan(args)
    else:
        ctx = Context(args.state_dir, args.bindings, args.bindings_sha256, args.policy, args.policy_sha256)
        if args.command in ("freeze", "bind-publication", "bind-public", "accept"):
            result = {"freeze": planning.freeze, "bind-publication": planning.bind_publication,
                      "bind-public": planning.bind_public, "accept": planning.accept}[args.command](ctx, args)
        elif args.command == "download-producer":
            result = downloads.ProducerDownloader(ctx).run(args)
        elif args.command == "download-stage":
            result = downloads.StageDownloader(ctx).run(args)
        elif args.command == "dispatch":
            result = dispatch.Dispatcher(ctx).run(args)
        elif args.command == "reconcile-dispatch":
            result = dispatch.Dispatcher(ctx).reconcile(args)
        elif args.command == "approve":
            result = Approval(ctx).run(args)
        elif args.command == "verify-public":
            result = PublicVerifier(ctx).run(args)
        else:
            from release_helpers.context import need
            verifier = BundleVerifier(ctx)
            bindings, core = verifier.setup()
            previous = {"path": str(args.previous_proof), "sha256": args.previous_sha256} if args.previous_proof else None
            pubs, metadata, proof = verifier.verify_bundle(bindings, core, args.bundle, args.stage, previous)
            if args.r2_phase:
                need(args.network and args.r2_output, "private R2 reads require --network and fresh receipt output")
                need(args.stage == {"before-reserve": "prepared", "before-sign": "reserved", "before-publish": "signed", "after-publish": "signed"}[args.r2_phase],
                     "R2 phase and bundle stage differ")
                prior = {"path": str(args.prior_r2), "sha256": args.prior_r2_sha256} if args.prior_r2 else None
                verifier.r2_read(bindings, core, args.bundle, pubs, metadata, args.r2_phase, prior, args.r2_output)
            result = {"proof": str(args.output), "sha256": ctx.emit(args.output, proof)}
    print(json.dumps(result, indent=2, sort_keys=True))
    return 1 if result.get("all_checks_passed") is False else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except OperatorError as error:
        print("Agent release operation refused: " + str(error), file=sys.stderr)
        sys.exit(1)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print("Agent release operation refused: " + type(error).__name__, file=sys.stderr)
        sys.exit(1)
