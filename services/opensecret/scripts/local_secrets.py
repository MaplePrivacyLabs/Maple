"""Resolve local OpenSecret credentials explicitly, without writing their values."""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import shutil
import sys

ROOT = Path(__file__).resolve().parent.parent
PROVIDER = "local_bws"
SCOPES = ("local", "continuum", "backend")
PROVIDER_KEYS = ("CONTINUUM_API_KEY", "TINFOIL_API_KEY", "KAGI_API_KEY", "BRAVE_API_KEY")


def clean_environment(inherited: dict[str, str], bws: str) -> dict[str, str]:
    # Preserve workspace ports, databases, toolchain and generated local auth.
    # Never let an ambient secret, provider, profile or endpoint select credentials.
    result = {
        k: v for k, v in inherited.items()
        if not k.startswith(("SECRETSPEC_", "BWS_")) and k not in PROVIDER_KEYS
    }
    result["SECRETSPEC_BWS_CLI_PATH"] = bws
    # Prevent legacy dotenv values from restoring unused provider credentials.
    result["BRAVE_API_KEY"] = ""
    result["OPENAI_API_KEY"] = ""
    return result


def command_args(secretspec: str, action: str, scope: str, command: list[str]) -> list[str]:
    args = [secretspec, "--file", str(ROOT / "secretspec.toml"),
            "--reason", f"OpenSecret local development {action} ({scope})"]
    if action == "login":
        return args + ["config", "provider", "login", PROVIDER]
    args += [action, "--provider", PROVIDER, "--profile", "default", "--scope", scope]
    if action == "check":
        return args + ["--no-prompt"]
    return args + ["--", *command]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("check", "run", "login"))
    parser.add_argument("scope", nargs="?", choices=SCOPES, default="local")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if args.action == "run" and not command:
        parser.error("run requires a command after --")
    if args.action != "run" and command:
        parser.error("only run accepts a command")
    if args.action == "login" and not sys.stdin.isatty():
        parser.error("login requires a local interactive terminal with a hidden token prompt")
    secretspec, bws = shutil.which("secretspec"), shutil.which("bws")
    if not secretspec or not bws:
        print("Enter the OpenSecret Nix development shell for SecretSpec and BWS.", file=sys.stderr)
        return 1
    # exec preserves the manager's process group and native exit/signal behavior.
    os.execve(secretspec, command_args(secretspec, args.action, args.scope, command),
              clean_environment(dict(os.environ), bws))
    return 1  # exec does not return


if __name__ == "__main__":
    raise SystemExit(main())
