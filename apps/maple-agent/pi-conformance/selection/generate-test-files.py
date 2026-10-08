"""Generate the portable curated test-file inventory and source manifest.

Usage: python3 -I -B generate-test-files.py [--check]
       [--output FILE] [--manifest-output FILE] [--upstream-root DIRECTORY]
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import sys

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from testcurate import CUR  # noqa: E402
from classes import SELECTED  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true', help='fail if the output needs regeneration')
    parser.add_argument('--output', type=Path, default=HERE / 'test-files.json')
    parser.add_argument('--manifest-output', type=Path, default=HERE / 'manifest.json')
    parser.add_argument('--upstream-root', type=Path, help='also verify each full upstream file against its pinned SHA-256')
    args = parser.parse_args()
    files = []
    for path, (classification, note) in sorted(CUR.items()):
        if classification not in {'T', 'P', 'E', 'R', 'A'}:
            raise ValueError(f'Unknown classification for {path}: {classification}')
        if not path.startswith('packages/') or not path.endswith('.test.ts') or '..' in Path(path).parts:
            raise ValueError(f'Invalid upstream test path: {path}')
        files.append(dict(path=path, classification=classification, note=note))
    selected_paths = [line.split('#', 1)[0].strip() for line in (HERE / 'selection.txt').read_text().splitlines()]
    selected_paths = [path for path in selected_paths if path]
    if len(selected_paths) != len(set(selected_paths)) or set(selected_paths) != set(SELECTED):
        raise ValueError('selection.txt must contain every SELECTED source exactly once')
    mappings = []
    targets = set()
    for upstream, entry in sorted(SELECTED.items()):
        prefix = re.escape(f'crates/{entry["crate"]}/src/')
        if not re.fullmatch(prefix + r'(?:[a-z_][a-z_0-9]*/)*[a-z_][a-z_0-9]*\.rs', entry['rust_module']):
            raise ValueError(f'Invalid Rust source path for {upstream}')
        if not re.fullmatch(r'[0-9a-f]{64}', entry['sha256']):
            raise ValueError(f'Invalid pinned source SHA-256 for {upstream}')
        if not upstream.startswith('packages/') or not upstream.endswith('.ts') or '..' in Path(upstream).parts:
            raise ValueError(f'Invalid upstream source path: {upstream}')
        if args.upstream_root is not None:
            actual = hashlib.sha256((args.upstream_root / upstream).read_bytes()).hexdigest()
            if actual != entry['sha256']:
                parser.exit(1, f'Pinned upstream source SHA-256 mismatch: {upstream}\n')
        target = (entry['crate'], entry['rust_module'])
        if target in targets:
            raise ValueError(f'Duplicate Rust source mapping: {target}')
        targets.add(target)
        if entry['cls'] not in {'Port', 'Adapt'} or entry.get('status', 'pending') not in {'pending', 'ported', 'adapted'}:
            raise ValueError(f'Invalid source classification or status: {upstream}')
        mappings.append({
            'upstream': upstream,
            'sha256': entry['sha256'],
            'class': entry['cls'],
            'crate': entry['crate'],
            'rust_module': entry['rust_module'],
            'adaptations': entry['adaptations'],
            'cut': entry.get('cut', []),
            'ranges': entry.get('ranges', []),
            'keep': entry.get('keep', []),
            'status': entry.get('status', 'pending'),
            'reason': entry['reason'],
        })
    for output, entries in [(args.output, files), (args.manifest_output, mappings)]:
        content = json.dumps(dict(schemaVersion=1, files=entries), indent=2, ensure_ascii=False) + '\n'
        if args.check:
            if not output.exists() or output.read_text(encoding='utf-8') != content:
                parser.exit(1, f'{output.name} is stale; run generate-test-files.py\n')
        else:
            output.write_text(content, encoding='utf-8')
    print(f'{len(files)} curated test files; {len(mappings)} source mappings')



if __name__ == '__main__':
    main()
