#!/usr/bin/env python3
"""Exercise a downloaded Agent macOS bundle's actual GUI without credentials."""

import argparse
import os
from pathlib import Path
import platform
import plistlib
import selectors
import subprocess
import sys
import tempfile
import time


class SmokeError(Exception):
    """A bounded diagnostic which deliberately excludes application output."""


def bundle_executable(bundle):
    """Accept a regular Agent bundle, never a link to an outside executable."""
    bundle = Path(bundle).absolute()
    if bundle.suffix != ".app" or bundle.is_symlink() or not bundle.is_dir():
        raise SmokeError("Expected a regular Agent .app directory")
    bundle = bundle.resolve(strict=True)
    contents = bundle / "Contents"
    binaries = contents / "MacOS"
    for directory in (contents, binaries):
        if directory.is_symlink() or not directory.is_dir():
            raise SmokeError("Agent bundle contains an invalid executable directory")
    executable = binaries / "maple-agent"
    plist = contents / "Info.plist"
    for file in (executable, plist):
        if file.is_symlink() or not file.is_file():
            raise SmokeError("Agent bundle requires regular executable and plist files")
    if not os.access(executable, os.X_OK):
        raise SmokeError("Agent bundle executable is not executable")
    try:
        with plist.open("rb") as source:
            metadata = plistlib.load(source)
    except (OSError, ValueError, plistlib.InvalidFileException) as error:
        raise SmokeError("Could not read the Agent bundle plist") from error
    if not isinstance(metadata, dict) or metadata.get("CFBundleExecutable") != "maple-agent":
        raise SmokeError("Unexpected Agent bundle executable identity")
    if metadata.get("CFBundleIdentifier") not in (
        "cloud.opensecret.maple.agent", "cloud.opensecret.maple.agent.dev"
    ):
        raise SmokeError("Unexpected Agent bundle identifier")
    return executable


def smoke_environment(parent, scratch):
    # Preserve OS/session basics, rather than copying signing, service, provider,
    # or CI credentials and attempting to enumerate every possible secret key.
    allowed = {"PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "__CF_USER_TEXT_ENCODING"}
    environment = {
        key: value for key, value in parent.items()
        if key in allowed or key.startswith("LC_")
    }
    environment.update(
        XDG_CONFIG_HOME=str(scratch / "config"),
        XDG_DATA_HOME=str(scratch / "data"),
        XDG_CACHE_HOME=str(scratch / "cache"),
        TMPDIR=str(scratch / "tmp"),
        MAPLE_DISABLE_UPDATE_CHECK="1",
        RUST_LOG="warn,maple_agent=debug",
    )
    for name in ("config", "data", "cache", "tmp"):
        (scratch / name).mkdir(mode=0o700)
    return environment


def stop_process(process):
    """Reap only the process created by this probe; never search by app name."""
    if process.poll() is not None:
        return
    try:
        process.terminate()
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def observe_gui(command, *, timeout=30.0, settle=1.0, parent_environment=None):
    """Observe a bounded real process; fixtures use this independently of macOS."""
    parent = os.environ if parent_environment is None else parent_environment
    with tempfile.TemporaryDirectory(prefix="maple-agent-gui-smoke-") as temporary:
        scratch = Path(temporary).resolve()
        environment = smoke_environment(parent, scratch)
        process = subprocess.Popen(
            command, cwd=scratch, env=environment, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, start_new_session=True,
        )
        try:
            os.set_blocking(process.stderr.fileno(), False)
            with selectors.DefaultSelector() as selector:
                selector.register(process.stderr, selectors.EVENT_READ)
                deadline = time.monotonic() + timeout
                window_open = first_render = False
                ready_since = None
                tail = b""
                total_bytes = 0
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        raise SmokeError("Agent GUI exited before completing the startup probe")
                    for key, _events in selector.select(timeout=0.05):
                        chunk = os.read(key.fd, 65536)
                        if not chunk:
                            selector.unregister(key.fileobj)
                            continue
                        total_bytes += len(chunk)
                        if total_bytes > 2 * 1024 * 1024:
                            raise SmokeError("Agent GUI exceeded the startup diagnostic output limit")
                        output = tail + chunk
                        window_open |= b"startup: window open at " in output
                        first_render |= b"startup: first render at " in output
                        if b"is implemented in both" in output:
                            raise SmokeError("Agent GUI loaded duplicate Objective-C/Swift runtimes")
                        tail = output[-1024:]
                    if window_open and first_render:
                        if ready_since is None:
                            ready_since = time.monotonic()
                        if time.monotonic() - ready_since >= settle:
                            if process.poll() is not None:
                                raise SmokeError("Agent GUI exited during the startup probe")
                            return
                raise SmokeError("Agent GUI did not open and render a window within the startup timeout")
        finally:
            try:
                stop_process(process)
            finally:
                process.stderr.close()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path)
    arguments = parser.parse_args(argv)
    try:
        if platform.system() != "Darwin":
            raise SmokeError("Agent macOS GUI verification requires a native macOS GUI session")
        executable = bundle_executable(arguments.bundle)
        observe_gui([str(executable)])
    except (SmokeError, OSError) as error:
        # Application stderr may contain sensitive data. Report only our
        # bounded diagnostics, never the subprocess output or its environment.
        diagnostic = str(error) if isinstance(error, SmokeError) else "Could not launch or clean up the Agent GUI probe"
        print(diagnostic, file=sys.stderr)
        return 1
    print("Verified downloaded Agent macOS window opened, entered rendering, and remained running")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
