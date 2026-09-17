#!/bin/sh
# Shared helpers for the Maple pre-commit hook and its component scripts.
# POSIX sh; sourced, not executed.

hook_log() {
    printf '[pre-commit] %s\n' "$*"
}

hook_fail() {
    printf '[pre-commit] ERROR: %s\n' "$*" >&2
    exit 1
}

# Run one check, printing the command first so a failure is easy to reproduce.
hook_run() {
    hook_log "\$ $*"
    "$@" || hook_fail "'$*' failed (cwd: $(pwd))"
}

# GUI Git clients on macOS may not inherit the shell PATH; also look in the
# standard Nix profile locations.
hook_find_nix() {
    if command -v nix >/dev/null 2>&1; then
        command -v nix
        return 0
    fi
    for candidate in /nix/var/nix/profiles/default/bin/nix "$HOME/.nix-profile/bin/nix"; do
        if [ -x "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

# True when the given component name is in MAPLE_HOOK_SELECTED.
hook_selected() {
    case " ${MAPLE_HOOK_SELECTED:-} " in
        *" $1 "*) return 0 ;;
        *) return 1 ;;
    esac
}

# Print the staged paths (repo-relative, one per line) matching a grep -E pattern.
hook_staged_matching() {
    grep -E "$1" "${MAPLE_HOOK_STAGED:-/dev/null}" || true
}

# Require a tool on PATH, with a hint that differs depending on whether the
# component's Nix shell was used.
hook_require() {
    for tool in "$@"; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            if [ "${MAPLE_HOOK_IN_NIX:-0}" = "1" ]; then
                hook_fail "'$tool' is missing from the component Nix shell."
            fi
            hook_fail "'$tool' is not on PATH. Install Nix so the hook can use the pinned toolchain, or install '$tool' yourself."
        fi
    done
}
