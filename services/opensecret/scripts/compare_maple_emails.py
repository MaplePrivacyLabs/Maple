#!/usr/bin/env python3
"""Compare HTML dumped by email::tests against the approved build.py output.

Run with MAPLE_EMAIL_DUMP_DIR=/tmp/maple-rust-html cargo test --locked
--all-features email::tests::dump_maple_html_for_preview_comparison -- --exact,
then: python3 scripts/compare_maple_emails.py /path/to/email-previews/build.py /tmp/maple-rust-html
"""
import argparse
import difflib
import importlib.util
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('preview_builder', type=Path)
parser.add_argument('rust_dir', type=Path)
args = parser.parse_args()
spec = importlib.util.spec_from_file_location('maple_preview', args.preview_builder)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.ASSET = module.PROD_ASSET
failed = False
for slug, _, _, builder in module.EMAILS:
    expected = builder(module.SAMPLE)['html']
    actual = (args.rust_dir / f'{slug}.html').read_text()
    if actual != expected:
        failed = True
        print(f'{slug}: DIFFERENT')
        print(''.join(difflib.unified_diff(expected.splitlines(True), actual.splitlines(True),
                                           fromfile='build.py', tofile='Rust')))
    else:
        print(f'{slug}: exact match ({len(actual)} bytes)')
raise SystemExit(1 if failed else 0)
