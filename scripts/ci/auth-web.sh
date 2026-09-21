#!/usr/bin/env bash
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/auth-common.sh"
use_auth_environment "${MAPLE_AUTH_ENVIRONMENT:-pr}"
prepare_auth_tooling
print_auth_source_provenance
install_auth_deps
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-315532800}"
cd "$AUTH_APP_DIR"
bun --no-env-file run build
test -f dist/index.html

# Normalize only this application's static artifact. No native build tree or
# Research metadata is involved. GNU tar and gzip come from the root CI shell.
find dist \( -name '.DS_Store' -o -name '._*' -o -name 'Thumbs.db' -o -name 'Desktop.ini' \) \
  -type f -exec rm -f -- {} +
repro_dir="$AUTH_APP_DIR/target/reproducibility"
mkdir -p "$repro_dir"
auth_archive="$repro_dir/maple-auth-dist.tar.gz"
(
  cd dist
  find . -mindepth 1 -print0 \
    | LC_ALL=C sort -z \
    | "${MAPLE_NIX_GNUTAR:-tar}" --null --no-recursion \
        --mtime="@${SOURCE_DATE_EPOCH}" --owner=0 --group=0 --numeric-owner -cf - -T -
) | "${MAPLE_NIX_GZIP:-gzip}" -n > "$auth_archive"
python3 -I "${AUTH_SCRIPT_DIR}/pages_auth_build.py" artifact --archive "$auth_archive"
python3 -I - "$auth_archive" "$repro_dir/auth-final.sha256" "$AUTH_REPO_ROOT" <<'PY'
from hashlib import sha256
from pathlib import Path
import sys
archive, manifest, root = map(Path, sys.argv[1:])
line = f"{sha256(archive.read_bytes()).hexdigest()}  {archive.relative_to(root)}\n"
manifest.write_text(line)
print(line, end="")
PY
