"""Strict whole-bundle and fixed GET-only private-state verification."""
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import signal
import stat
import time
from .context import need, sha, read_regular, decode, SCHEMA_BUNDLE, SCHEMA_R2, SCHEMA_FREEZE
NAMES = ("manifest.json", "linux-manifest.json", "windows-manifest.json")
PHASES = ("before-reserve", "before-sign", "before-publish", "after-publish")

class BundleVerifier:
    def __init__(self, ctx):
        self.ctx = ctx

    def pinned(self, reference):
        return self.ctx.pinned(reference)

    def setup(self, path=None):
        ctx = self.ctx
        bindings = ctx.bindings
        need(bool(bindings.get("prepared_files")), "prepared file freeze required")
        for key in ("qualification", "freeze", "selected_publication"):
            ctx.pinned(bindings[key])
        selected = decode(ctx.pinned(bindings["selected_publication"]))
        need(all(selected.get(key) == value for key, value in ctx.publication.items()), "selected publication differs")
        need(selected.get("payload_source") == ctx.source and selected.get("producer_run") == ctx.producer_run
             and selected.get("producer_attempt") == ctx.producer_attempt and selected.get("version") == ctx.version,
             "selected publication payload differs")
        return bindings, ctx.core

    def verify_bundle(self, bindings, core, bundle, stage, previous):
        need(bundle.is_dir() and (not bundle.is_symlink()), 'invalid bundle directory')
        publications = core.bundle_manifests(bundle)
        need(tuple((item['name'] for item in publications)) == NAMES, 'normal release must include all three manifests')
        manifest = publications[0]['manifest']
        identity = bindings['producer_identity']
        need(all((manifest[key] == identity[key] for key in ('version', 'build', 'source_sha', 'pub_date'))), 'release identity differs')
        notes = self.ctx.pinned(bindings['notes']).decode('utf-8')
        need(manifest['notes'] == notes, 'exact release notes differ')
        trust = core.decode_json(core.read_bytes(bundle / 'trust.json', core.MAX_JSON))
        expected = {key: identity[key] for key in ('run_id', 'run_attempt', 'source_sha', 'build', 'pub_date')}
        expected['artifacts'] = {platform: item['name'] for platform, item in bindings['artifacts'].items()}
        need(trust == expected, 'producer trust receipt differs')
        need(set(expected['artifacts']) == {'macos', 'linux', 'windows'}, 'missing selected platform artifact')
        for platform, arch in (('macos', 'macos-aarch64'), ('linux', 'linux-x86_64'), ('windows', 'windows-x86_64')):
            item = bindings['artifacts'][platform]
            need(item['name'] == f"maple-agent-prod-{arch}-{identity['run_id']}" and type(item['id']) is int and (item['id'] > 0) and (type(item['size']) is int) and (item['size'] > 0) and core.DIGEST.fullmatch(item['zip_sha256']), 'invalid final artifact pin')
        need(core.public_key_text(bundle / 'public-key.pub') == core.public_key_text(bindings['public_key']), 'bundle public key differs')
        payloads = core.bundle_records(bundle, manifest)
        metadata_bytes, metadata = core.bundle_metadata(bundle, manifest)
        need(core.read_bytes(bundle / 'metadata/provenance.json', core.MAX_JSON) == core.durable_provenance(trust), 'durable provenance differs')
        roots = set(NAMES) | {'trust.json', 'public-key.pub', 'metadata-inventory.json'}
        base = roots | {str(path.relative_to(bundle)) for path, _ in payloads + metadata}
        envelopes = {item['envelope_name'] for item in publications}
        expected_files = base | ({'reservation.json'} if stage != 'prepared' else set()) | (envelopes if stage == 'signed' else set())
        actual = set()
        for path in bundle.rglob('*'):
            relative = str(path.relative_to(bundle))
            need(not path.is_symlink(), 'symlink in bundle')
            if path.is_dir():
                need(relative in {'payloads', 'metadata'}, 'unexpected bundle directory')
            else:
                need(stat.S_ISREG(path.lstat().st_mode), 'special file in bundle')
                actual.add(relative)
        need(actual == expected_files, 'whole bundle inventory differs')
        files = {str(path.relative_to(bundle)): {'sha256': record['sha256'], 'size': record['size']} for path, record in payloads + metadata}
        for name in roots | expected_files - base:
            data = core.read_bytes(bundle / name, core.MAX_JSON)
            files[name] = {'sha256': sha(data), 'size': len(data)}
        need({name: files[name] for name in base} == bindings['prepared_files'], 'prepared freeze bytes differ')
        reservation = core.encode_json(core.reservation_receipt(publications, metadata_bytes))
        if stage != 'prepared':
            need(core.read_bytes(bundle / 'reservation.json', core.MAX_MANIFEST) == reservation, 'local reservation proof differs')
        if stage == 'signed':
            executable = Path(self.ctx.tool('minisign'))
            need(executable.name == 'minisign', 'pinned verifier executable name differs')
            previous_path = os.environ.get('PATH')
            try:
                os.environ['PATH'] = str(executable.parent)
                for item in publications:
                    data = core.read_bytes(bundle / item['envelope_name'], core.MAX_ENVELOPE)
                    core.minisign_verify(data, bindings['public_key'])
                    need(core.parse_envelope(data)[0]['manifest'].encode() == item['data'], 'signed document differs from prepared bytes')
            finally:
                if previous_path is None:
                    os.environ.pop('PATH', None)
                else:
                    os.environ['PATH'] = previous_path
        if stage == 'prepared':
            need(previous is None, 'prepared bundle must start the proof chain')
        else:
            need(previous is not None, 'missing predecessor proof')
            prior = core.decode_json(self.ctx.pinned(previous))
            need(prior['schema'] == SCHEMA_BUNDLE and prior['bindings_sha256'] == self.ctx.bindings_sha256 and prior['tooling_sha256'] == self.ctx.tooling_sha256 and prior['policy_sha256'] == self.ctx.policy_sha256 and (prior['stage'] == ('prepared' if stage == 'reserved' else 'reserved')), 'wrong predecessor proof')
            need(all((files.get(name) == record for name, record in prior['files'].items())), 'predecessor bytes changed')
            allowed_new = {'reservation.json'} if stage == 'reserved' else envelopes
            need(set(files) - set(prior['files']) == allowed_new, 'unexpected stage inventory delta')
        proof = {'schema': SCHEMA_BUNDLE, 'bindings_sha256': self.ctx.bindings_sha256, 'tooling_sha256': self.ctx.tooling_sha256, 'policy_sha256': self.ctx.policy_sha256, 'stage': stage, 'files': files, 'producer': identity, 'publication': bindings['publication_identity'], 'previous_proof_sha256': previous['sha256'] if previous else None, 'core_sha256': self.ctx.core_sha256, 'primary_key_sha256': self.ctx.primary_sha256, 'reservation_sha256': sha(reservation), 'verified_primary_signatures': sorted(envelopes) if stage == 'signed' else []}
        return (publications, metadata_bytes, proof)

    def r2_read(self, bindings, core, bundle, publications, metadata, phase, prior_reference, output):
        need(phase in PHASES, 'unknown private-state phase')
        need(phase != 'before-reserve' or prior_reference is None, 'initial private proof cannot have a predecessor')
        need(stat.S_IMODE(self.ctx.route.lstat().st_mode) == 384 and sha(read_regular(self.ctx.route)) == self.ctx.route_sha256, 'references-only credential route changed')
        baseline = core.decode_json(self.ctx.pinned(bindings['previous_private_r2']))['objects']
        self.ctx.pinned(bindings['previous_root_acceptance'])
        for key, reference in bindings['previous_signed_envelopes'].items():
            data = self.ctx.pinned(reference)
            need(baseline[key]['sha256'] == sha(data) and baseline[key]['size'] == len(data), 'accepted predecessor envelope differs')
        current = [item['pointer'] for item in publications]
        old = [f"stable/releases/{self.ctx.bindings['previous']['version']}/{name}" for name in NAMES]
        new = f'stable/releases/{self.ctx.version}/'
        reservations = {new + item['reservation']: item['data'] for item in publications}
        reservations[new + 'publication-metadata.json'] = metadata
        complete = {new + item['name']: core.read_bytes(bundle / item['envelope_name'], core.MAX_ENVELOPE) if phase == 'after-publish' else None for item in publications}
        keys = current + old + list(reservations) + list(complete)
        need(len(keys) == len(set(keys)) == 13, 'fixed metadata GET inventory differs')
        prior = None
        if phase != 'before-reserve':
            need(prior_reference is not None, 'missing predecessor R2 proof')
            prior = core.decode_json(self.ctx.pinned(prior_reference))
            need(prior['schema'] == SCHEMA_R2 and prior['passed'] is True and (prior['bindings_sha256'] == self.ctx.bindings_sha256 and prior['tooling_sha256'] == self.ctx.tooling_sha256 and prior['policy_sha256'] == self.ctx.policy_sha256) and (prior['phase'] == PHASES[PHASES.index(phase) - 1]), 'wrong predecessor R2 proof')
        expected = {}
        for key in old + current:
            expected[key] = baseline[key]
        for key, data in reservations.items():
            expected[key] = None if phase == 'before-reserve' else {'sha256': sha(data), 'size': len(data)}
        for key, data in complete.items():
            expected[key] = None if data is None else {'sha256': sha(data), 'size': len(data)}
        if phase == 'after-publish':
            for item in publications:
                data = core.read_bytes(bundle / item['envelope_name'], core.MAX_ENVELOPE)
                expected[item['pointer']] = {'sha256': sha(data), 'size': len(data)}
        if phase in {'before-publish', 'after-publish'}:
            for key in reservations:
                expected[key]['etag'] = prior['objects'][key]['etag']
        deadline = time.monotonic() + 180

        class GetOnly(core.R2Store):

            def request(self, method, key, data=None, content_hash=None, size=None, condition=None, content_type=None, maximum=core.MAX_JSON, consumer=None):
                need(method == 'GET' and self.count < len(keys) and (key == keys[self.count]) and all((value is None for value in (data, content_hash, size, condition, content_type, consumer))), 'only the fixed ordered metadata GETs are allowed')
                need(maximum in {core.MAX_MANIFEST, core.MAX_ENVELOPE} and time.monotonic() < deadline, 'R2 read bound exceeded')
                self.count += 1
                return super().request(method, key, maximum=maximum)
        store = GetOnly(self.ctx.endpoint, self.ctx.bucket)
        store.count = 0
        opener = store.opener

        class BoundedOpener:

            def open(self, request, timeout):
                need(time.monotonic() < deadline, 'R2 read deadline exceeded')
                return opener.open(request, timeout=min(20, deadline - time.monotonic()))
        store.opener = BoundedOpener()
        objects = {}
        receipt = {'schema': SCHEMA_R2, 'bindings_sha256': self.ctx.bindings_sha256, 'tooling_sha256': self.ctx.tooling_sha256, 'policy_sha256': self.ctx.policy_sha256, 'phase': phase, 'started_at': datetime.now(timezone.utc).isoformat(), 'route_sha256': self.ctx.route_sha256, 'previous_proof_sha256': prior_reference['sha256'] if prior_reference else None, 'objects': objects, 'passed': False}

        def timed_out(_signum, _frame):
            raise TimeoutError('absolute R2 read deadline exceeded')
        old_handler = signal.signal(signal.SIGALRM, timed_out)
        previous_timer = signal.setitimer(signal.ITIMER_REAL, max(0.001, deadline - time.monotonic()))
        try:
            for key in keys:
                result = store.get(key, core.MAX_MANIFEST if key in reservations else core.MAX_ENVELOPE)
                need(time.monotonic() < deadline, 'absolute R2 read deadline exceeded')
                need(result is None or (isinstance(result[1], str) and result[1] and all((ord(char) >= 32 and ord(char) != 127 for char in result[1]))), 'invalid opaque R2 ETag')
                objects[key] = None if result is None else {'sha256': sha(result[0]), 'size': len(result[0]), 'etag': result[1]}
            receipt['passed'] = all((objects[key] is None if record is None else objects[key] is not None and all((objects[key].get(field) == value for field, value in record.items())) for key, record in expected.items()))
        finally:
            signal.setitimer(signal.ITIMER_REAL, 0)
            signal.signal(signal.SIGALRM, old_handler)
            if previous_timer[0] > 0:
                elapsed = time.monotonic() - (deadline - 180)
                signal.setitimer(signal.ITIMER_REAL, max(0.001, previous_timer[0] - elapsed), previous_timer[1])
            receipt['request_count'] = store.count
            receipt['completed_at'] = datetime.now(timezone.utc).isoformat()
            self.ctx.emit(output, receipt)
        need(receipt['passed'], 'R2 state differs; preserve receipt and inspect without an automatic retry')
