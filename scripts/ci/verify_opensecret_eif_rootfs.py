#!/usr/bin/env python3
"""Inspect the user ramdisk input of an unsigned PR EIF candidate.

This is diagnostic build evidence, not an EIF attestation or deployment handoff.
The release workflow remains the only producer of attested EIF artifacts.
"""

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import posixpath
import re
import shlex
import shutil
import ssl
import stat
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]
COMPONENT = ROOT / "services/opensecret"
sys.path.insert(0, str(COMPONENT / "scripts"))
from test_enclave_ca_bundle import (  # noqa: E402
    COMODO_AAA,
    EMSIGN_C1,
    EMSIGN_C3,
    EMSIGN_G3,
    fingerprints,
)


STORE_PATH = r"/nix/store/[0-9a-z]{32}"
USER_RAMDISK = re.compile(rf"^{STORE_PATH}-user-initramfs\.cpio\.gz$")
USER_RAMDISK_DRV = re.compile(r"^[0-9a-z]{32}-user-initramfs\.cpio\.gz\.drv$")
EIF_OUTPUT = re.compile(rf"^{STORE_PATH}-opensecret-eif-(dev|prod)$")
ENTRYPOINT_STORE = re.compile(
    rf"^rootfs{STORE_PATH}-entrypoint/bin/entrypoint$"
)
CA_STORE_FILE = re.compile(
    rf"^rootfs({STORE_PATH}-nss-cacert-3\.131\.1)"
    r"/etc/ssl/certs/ca-bundle\.crt$"
)
CA_LINK = re.compile(
    rf"(?m)^\s*ln -sf ({STORE_PATH}-nss-cacert-3\.131\.1)"
    r"/etc/ssl/certs/ca-bundle\.crt /etc/ssl/certs/ca-bundle\.crt\s*$"
)
MEASUREMENT = re.compile(r"[0-9a-f]{96}")
SOURCE_SHA = re.compile(r"[0-9a-f]{40}")
MAX_ARCHIVE_BYTES = 4 * 1024 * 1024 * 1024
MAX_ENTRIES = 250_000
MAX_CAPTURE_BYTES = 4 * 1024 * 1024
AAA_ROOT = ROOT / "scripts/ci/fixtures/eif/comodo-aaa-services-root.pem"
AAA_ROOT_SHA256 = "a5ddabd1602ae1c66ce11ad078e734cc473dcb8e9f573037832d8536ae3de90b"


class VerificationError(Exception):
    """A candidate failed the diagnostic integrity or trust-path check."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise VerificationError(message)


def regular_file(path: Path) -> bool:
    try:
        return stat.S_ISREG(path.lstat().st_mode)
    except FileNotFoundError:
        return False


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_exact(file, size: int) -> bytes:
    data = file.read(size)
    require(len(data) == size, "truncated user ramdisk CPIO")
    return data


def skip_exact(file, size: int) -> None:
    while size:
        chunk = file.read(min(size, 1024 * 1024))
        require(bool(chunk), "truncated user ramdisk CPIO entry")
        size -= len(chunk)


def newc_entries(archive: Path) -> dict[str, tuple[int, bytes]]:
    """Read only the selected command, entrypoint and CA from a bounded archive."""
    captured: dict[str, tuple[int, bytes]] = {}
    seen: set[str] = set()
    uncompressed = 0
    with gzip.open(archive, "rb") as file:
        for entry_number in range(MAX_ENTRIES):
            header = read_exact(file, 110)
            uncompressed += len(header)
            require(header[:6] == b"070701", "unsupported user ramdisk CPIO format")
            try:
                fields = [int(header[offset : offset + 8], 16) for offset in range(6, 110, 8)]
            except ValueError as error:
                raise VerificationError("malformed user ramdisk CPIO header") from error
            mode, size, name_size = fields[1], fields[6], fields[11]
            require(1 <= name_size <= 4096, "invalid user ramdisk CPIO name size")
            require(size <= MAX_ARCHIVE_BYTES, "oversized user ramdisk CPIO entry")
            raw_name = read_exact(file, name_size)
            uncompressed += name_size
            require(raw_name.endswith(b"\0"), "unterminated user ramdisk CPIO name")
            try:
                name = raw_name[:-1].decode("utf-8")
            except UnicodeDecodeError as error:
                raise VerificationError("non-UTF-8 user ramdisk CPIO name") from error
            require(name not in seen, "duplicate user ramdisk CPIO path")
            seen.add(name)
            name_padding = (-(110 + name_size)) % 4
            skip_exact(file, name_padding)
            uncompressed += name_padding
            require(uncompressed + size <= MAX_ARCHIVE_BYTES, "user ramdisk exceeds size limit")
            if name == "TRAILER!!!":
                require(size == 0, "malformed user ramdisk CPIO trailer")
                tail = file.read(4097)
                require(len(tail) <= 4096 and not any(tail) and not file.read(1),
                        "unexpected data after user ramdisk CPIO trailer")
                return captured
            if (name in ("cmd", "rootfs/bin/entrypoint")
                    or ENTRYPOINT_STORE.fullmatch(name) or CA_STORE_FILE.fullmatch(name)):
                require(size <= MAX_CAPTURE_BYTES, "oversized inspected user ramdisk entry")
                data = read_exact(file, size)
                captured[name] = (mode, data)
            else:
                skip_exact(file, size)
            uncompressed += size
            data_padding = (-size) % 4
            skip_exact(file, data_padding)
            uncompressed += data_padding
        raise VerificationError("too many user ramdisk CPIO entries")


def inspect_archive(archive: Path) -> tuple[str, bytes]:
    entries = newc_entries(archive)
    require("cmd" in entries, "Nitro init command missing from user ramdisk")
    cmd_mode, cmd_data = entries["cmd"]
    require(stat.S_ISREG(cmd_mode) and cmd_data in (b"/bin/entrypoint", b"/bin/entrypoint\n"),
            "Nitro init command does not select /bin/entrypoint")
    require("rootfs/bin/entrypoint" in entries, "rootfs entrypoint missing from user ramdisk")
    entry_mode, entry_data = entries["rootfs/bin/entrypoint"]
    if stat.S_ISLNK(entry_mode):
        try:
            target = entry_data.decode("utf-8")
        except UnicodeDecodeError as error:
            raise VerificationError("invalid entrypoint symlink") from error
        resolved = (
            "rootfs" + target if target.startswith("/")
            else posixpath.normpath(posixpath.join("rootfs/bin", target))
        )
        require(bool(ENTRYPOINT_STORE.fullmatch(resolved)), "entrypoint symlink leaves reviewed store path")
        require(resolved in entries, "entrypoint symlink target missing from user ramdisk")
        entry_mode, entry_data = entries[resolved]
    require(stat.S_ISREG(entry_mode), "rootfs entrypoint is not a regular script")
    try:
        entrypoint = entry_data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise VerificationError("entrypoint script is not UTF-8") from error
    links = CA_LINK.findall(entrypoint)
    require(len(links) == 1, "entrypoint does not select one NSS 3.131.1 CA bundle")
    for variable in ("SSL_CERT_FILE", "AWS_CA_BUNDLE"):
        require(
            re.search(
                rf"(?m)^\s*export {variable}=/etc/ssl/certs/ca-bundle\.crt\s*$",
                entrypoint,
            ) is not None,
            f"entrypoint does not export {variable} to its CA bundle",
        )
    ca_store_path = links[0]
    ca_archive_path = f"rootfs{ca_store_path}/etc/ssl/certs/ca-bundle.crt"
    require(ca_archive_path in entries, "effective CA bundle missing from user ramdisk")
    ca_mode, ca_bundle = entries[ca_archive_path]
    require(stat.S_ISREG(ca_mode), "effective CA bundle is not a regular file")
    require(bool(ca_bundle), "effective CA bundle is empty")
    return ca_store_path, ca_bundle


def eif_derivation(mode: str) -> tuple[Path, Path, Path]:
    result = subprocess.run(
        ["nix", "derivation", "show", "--no-update-lock-file", f".?submodules=1#eif-{mode}"],
        cwd=COMPONENT,
        check=True,
        capture_output=True,
        text=True,
    )
    data = json.loads(result.stdout)
    derivations = data.get("derivations")
    require(isinstance(derivations, dict) and len(derivations) == 1, "ambiguous EIF derivation")
    derivation = next(iter(derivations.values()))
    env = derivation.get("env", {})
    out = env.get("out")
    require(isinstance(out, str) and EIF_OUTPUT.fullmatch(out) is not None, "unexpected EIF output path")
    require(out.endswith(f"-opensecret-eif-{mode}"), "EIF derivation mode mismatch")
    args = env.get("ramdisksArgs")
    require(isinstance(args, str), "EIF derivation lacks ramdisksArgs")
    parts = shlex.split(args)
    require(len(parts) == 4 and parts[0] == parts[2] == "--ramdisk", "unexpected EIF ramdisk inputs")
    require(bool(USER_RAMDISK.fullmatch(parts[3])), "EIF user ramdisk input is unexpected")
    inputs = derivation.get("inputs", {})
    require(isinstance(inputs, dict), "EIF derivation has malformed inputs")
    input_drvs = inputs.get("drvs", {})
    require(isinstance(input_drvs, dict), "EIF derivation lacks direct input derivations")
    user_drvs = []
    for key, value in input_drvs.items():
        name = key.removeprefix("/nix/store/")
        if USER_RAMDISK_DRV.fullmatch(name):
            require(key in (name, f"/nix/store/{name}") and isinstance(value, dict),
                    "EIF user ramdisk input derivation is malformed")
            user_drvs.append((name, value))
    require(len(user_drvs) == 1, "EIF derivation lacks one direct user ramdisk derivation")
    user_drv_name, user_drv_input = user_drvs[0]
    require(user_drv_input.get("outputs") == ["out"],
            "EIF user ramdisk derivation has unexpected outputs")
    return Path(out), Path(parts[3]), Path("/nix/store") / user_drv_name


def realize_user_ramdisk(user_archive: Path, user_drv: Path) -> None:
    """Materialize a direct EIF input even when its final EIF was substituted."""
    result = subprocess.run(
        ["nix", "derivation", "show", "--no-update-lock-file", str(user_drv)],
        cwd=COMPONENT, check=True, capture_output=True, text=True,
    )
    derivations = json.loads(result.stdout).get("derivations")
    require(isinstance(derivations, dict) and len(derivations) == 1,
            "ambiguous user ramdisk derivation")
    key, derivation = next(iter(derivations.items()))
    require(key in (user_drv.name, str(user_drv)), "Nix returned another user ramdisk derivation")
    require(derivation.get("env", {}).get("out") == str(user_archive)
            and derivation.get("outputs", {}).get("out", {}).get("path") == user_archive.name,
            "direct EIF input derivation does not produce the selected user ramdisk")
    realized = subprocess.run(
        ["nix", "build", "--no-link", "--no-update-lock-file", "--print-out-paths",
         f"{user_drv}^out"],
        cwd=COMPONENT, check=True, stdout=subprocess.PIPE, text=True,
    )
    require(realized.stdout == f"{user_archive}\n", "Nix realized a different user ramdisk")
    require(regular_file(user_archive), "EIF user ramdisk is missing after explicit realization")


def verify_aaa_tls_policy(openssl: str, bundle: Path, full_fingerprints: set[str]) -> str:
    """Require any retained AAA root to be explicitly rejected for OpenSSL TLS."""
    # This fixture is the public AAA root from the pinned Nixpkgs cacert.unbundled.
    # A direct CA-purpose probe tests its trust attributes, not a server leaf.
    require(regular_file(AAA_ROOT) and sha256_file(AAA_ROOT) == AAA_ROOT_SHA256,
            "pinned Comodo AAA certificate fixture missing or changed")
    aaa_der = ssl.PEM_cert_to_DER_cert(AAA_ROOT.read_text())
    require(hashlib.sha256(aaa_der).hexdigest().upper() == COMODO_AAA,
            "Comodo AAA certificate fixture has the wrong fingerprint")
    # rustls-native-certs reads ordinary CERTIFICATE PEM blocks from SSL_CERT_FILE;
    # it does not import NSS's TRUSTED CERTIFICATE purpose metadata. AAA must
    # never appear as an ordinary PEM in the effective runtime bundle.
    ordinary_pems = re.findall(
        rb"-----BEGIN CERTIFICATE-----\s+.*?-----END CERTIFICATE-----",
        bundle.read_bytes(), re.DOTALL,
    )
    require(len(ordinary_pems) >= 100, "effective CA bundle has too few ordinary TLS roots")
    ordinary_fingerprints = {
        hashlib.sha256(ssl.PEM_cert_to_DER_cert(pem.decode("ascii"))).hexdigest().upper()
        for pem in ordinary_pems
    }
    require(COMODO_AAA not in ordinary_fingerprints,
            "Comodo AAA entered the ordinary TLS root PEMs")
    if COMODO_AAA not in full_fingerprints:
        return "absent"
    rejected = subprocess.run(
        [openssl, "verify", "-purpose", "sslserver", "-CAfile", str(bundle), str(AAA_ROOT)],
        capture_output=True,
    )
    require(rejected.returncode != 0 and b"certificate rejected" in rejected.stderr,
            "effective CA bundle did not explicitly reject Comodo AAA for OpenSSL TLS")
    return "retained_rejected_for_openssl_tls"


def verify_candidate(mode: str, candidate: Path, source_sha: str, openssl: str,
                     store_root: Path = Path("/")) -> dict:
    require(mode in ("dev", "prod"), "expected dev or prod")
    require(SOURCE_SHA.fullmatch(source_sha) is not None, "expected a full lowercase source SHA")
    require(candidate.is_dir() and not candidate.is_symlink(), "candidate directory missing or symlinked")
    expected = ("image.eif", "pcr.json", "SHA256SUMS", "handoff.json")
    require(all(regular_file(candidate / name) for name in expected), "candidate has missing or nonregular files")
    eif_output, user_archive, user_drv = eif_derivation(mode)
    require(regular_file(eif_output / "image.eif") and regular_file(eif_output / "pcr.json"),
            "EIF derivation output is missing or nonregular")
    realize_user_ramdisk(user_archive, user_drv)
    eif_sha = sha256_file(candidate / "image.eif")
    pcr_sha = sha256_file(candidate / "pcr.json")
    require(eif_sha == sha256_file(eif_output / "image.eif"), "candidate EIF differs from derivation output")
    require(pcr_sha == sha256_file(eif_output / "pcr.json"), "candidate PCR differs from derivation output")
    handoff_sha = sha256_file(candidate / "handoff.json")
    # The build script's exact output: the attestation covers all three files.
    sums = (candidate / "SHA256SUMS").read_text()
    require(sums == f"{eif_sha}  image.eif\n{pcr_sha}  pcr.json\n{handoff_sha}  handoff.json\n",
            "candidate SHA256SUMS mismatch")
    pcr = json.loads((candidate / "pcr.json").read_text())
    require(isinstance(pcr, dict) and set(pcr) == {"HashAlgorithm", "PCR0", "PCR1", "PCR2"},
            "candidate PCR JSON has unexpected fields")
    require(isinstance(pcr["HashAlgorithm"], str) and all(
        isinstance(pcr[key], str) and MEASUREMENT.fullmatch(pcr[key]) and pcr[key] != "0" * 96
        for key in ("PCR0", "PCR1", "PCR2")
    ), "candidate measurements are malformed")
    handoff = json.loads((candidate / "handoff.json").read_text())
    require(isinstance(handoff, dict)
            and set(handoff) == {"environment", "source_sha", "eif_sha256", "measurements", "approval_needed"}
            and handoff.get("environment") == mode
            and handoff.get("source_sha") == source_sha
            and handoff.get("eif_sha256") == eif_sha
            and handoff.get("measurements") == pcr,
            "candidate handoff does not match its bytes and PR merge ref")
    ca_store_path, ca_bundle = inspect_archive(user_archive)
    ca_file = store_root / ca_store_path.lstrip("/") / "etc/ssl/certs/ca-bundle.crt"
    ca_sha = hashlib.sha256(ca_bundle).hexdigest()
    ca_store_comparison = "not_materialized"
    if ca_file.exists() or ca_file.is_symlink():
        require(regular_file(ca_file), "effective CA bundle in Nix store is nonregular")
        require(ca_sha == sha256_file(ca_file), "archived CA bundle differs from its Nix store input")
        ca_store_comparison = "matched"
    with tempfile.TemporaryDirectory(prefix="opensecret-eif-ca-") as tmp:
        extracted = Path(tmp) / "ca-bundle.crt"
        extracted.write_bytes(ca_bundle)
        trusted = fingerprints(openssl, extracted)
        aaa_tls_policy = verify_aaa_tls_policy(openssl, extracted, trusted)
    require(not ({EMSIGN_C1, EMSIGN_C3, EMSIGN_G3} & trusted),
            "effective CA bundle still contains an affected emSign root")
    return {
        "kind": "PR_DIAGNOSTIC_UNAPPROVED",
        "environment": mode,
        "source_sha": source_sha,
        "eif_sha256": eif_sha,
        "pcr0": pcr["PCR0"],
        "pcr1": pcr["PCR1"],
        "pcr2": pcr["PCR2"],
        "user_ramdisk": str(user_archive),
        "user_ramdisk_sha256": sha256_file(user_archive),
        "ca_store_path": ca_store_path,
        "ca_bundle_sha256": ca_sha,
        "ca_store_comparison": ca_store_comparison,
        "ca_certificate_count": len(trusted),
        "comodo_aaa_tls_policy": aaa_tls_policy,
    }


def append_summary(path: Path, report: dict) -> None:
    with path.open("a") as summary:
        summary.write(f"\n### Unsigned {report['environment']} EIF PR rootfs diagnostic\n\n")
        summary.write("PR diagnostic only: the source SHA is the PR merge ref. No artifact is uploaded or approved.\n\n")
        summary.write("| Field | Value |\n| --- | --- |\n")
        for key in ("source_sha", "eif_sha256", "pcr0", "pcr1", "pcr2", "user_ramdisk_sha256",
                    "ca_bundle_sha256", "ca_store_comparison", "ca_certificate_count",
                    "comodo_aaa_tls_policy"):
            summary.write(f"| {key} | `{report[key]}` |\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("dev", "prod"))
    parser.add_argument("candidate", type=Path)
    parser.add_argument("source_sha")
    args = parser.parse_args()
    openssl = shutil.which("openssl")
    require(openssl is not None, "OpenSSL CLI unavailable for CA fingerprint verification")
    report = verify_candidate(args.mode, args.candidate, args.source_sha, openssl)
    if summary_path := os.environ.get("GITHUB_STEP_SUMMARY"):
        append_summary(Path(summary_path), report)
    print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except (VerificationError, OSError, ValueError, json.JSONDecodeError, subprocess.CalledProcessError) as error:
        print(f"EIF rootfs diagnostic failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
