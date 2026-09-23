import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { createPrivateKey, sign, X509Certificate } from 'node:crypto';
import fs from 'node:fs';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { request as httpRequest } from 'node:http';
import { syncBuiltinESMExports } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { after, mock, test } from 'node:test';
import {
  createFixtureServer, createTransactionVerifier, FIXTURE_ACCOUNT_TOKEN,
  FIXTURE_BUNDLE_ID, FIXTURE_PRODUCTS,
} from './fixture-server.mjs';

const output = mkdtempSync(join(tmpdir(), 'maple-storekit-fixture-test-'));
after(() => rmSync(output, { recursive: true, force: true }));

function signingFixture(name) {
  const keyPath = join(output, `${name}.key`);
  const certPath = join(output, `${name}.pem`);
  execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt',
    'ec_paramgen_curve:prime256v1', '-nodes', '-keyout', keyPath,
    '-out', certPath, '-days', '1', '-subj', '/CN=Local StoreKit Unit Test'], { stdio: 'ignore' });
  return {
    privateKey: createPrivateKey(readFileSync(keyPath)),
    certificateBytes: readFileSync(certPath),
    certificate: new X509Certificate(readFileSync(certPath)),
  };
}

const signer = signingFixture('trusted');
const otherSigner = signingFixture('untrusted');
function signedTransaction(changes = {}, source = signer) {
  const header = Buffer.from(JSON.stringify({ alg: 'ES256', x5c: [source.certificate.raw.toString('base64')] })).toString('base64url');
  const payload = Buffer.from(JSON.stringify({
    transactionId: '18446744073709551615', originalTransactionId: '9007199254740993',
    bundleId: FIXTURE_BUNDLE_ID, environment: 'Xcode', productId: FIXTURE_PRODUCTS[0],
    appAccountToken: FIXTURE_ACCOUNT_TOKEN, ...changes,
  })).toString('base64url');
  const signature = sign('sha256', Buffer.from(`${header}.${payload}`),
    { key: source.privateKey, dsaEncoding: 'ieee-p1363' }).toString('base64url');
  return `${header}.${payload}.${signature}`;
}

let sequence = 0;
async function runningFixture(t, journalPath = join(output, `journal-${sequence++}.jsonl`), bootstrapOnly = false) {
  const server = createFixtureServer({ localFixture: true,
    certificateBytes: bootstrapOnly ? undefined : signer.certificateBytes, journalPath, bootstrapOnly });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  let stopped = false;
  const close = async () => {
    if (stopped) return;
    stopped = true;
    await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  };
  t.after(close);
  const base = `http://127.0.0.1:${server.address().port}`;
  const request = async (path, options = {}) => {
    const { headers, body, ...rest } = options;
    const response = await fetch(`${base}${path}`, {
      ...rest,
      headers: { 'X-Maple-StoreKit-Fixture': '1', ...(body === undefined ? {} : { 'Content-Type': 'application/json' }), ...headers },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await response.text();
    return { status: response.status, headers: response.headers, body: text ? JSON.parse(text) : null };
  };
  return { journalPath, request, close, base };
}

const transactionRoute = '/v1/maple/subscription/apple/transactions';
const tokenRoute = '/v1/maple/subscription/apple/account-token';
const controlHeaders = { 'X-Maple-Fixture-Control': 'storekit-harness' };

test('startup requires explicit local-fixture mode, including the CLI', () => {
  assert.throws(() => createFixtureServer({}), /explicit local-fixture/);
  const result = spawnSync(process.execPath, [new URL('./fixture-server.mjs', import.meta.url).pathname], { encoding: 'utf8' });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /--local-fixture is required/);
});

test('explicit bootstrap mode issues the fixture account token but can never acknowledge a transaction', async (t) => {
  const fixture = await runningFixture(t, undefined, true);
  assert.deepEqual((await fixture.request(tokenRoute)).body, { app_account_token: FIXTURE_ACCOUNT_TOKEN });
  const result = await fixture.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: signedTransaction() },
  });
  assert.equal(result.status, 503);
  assert.equal(result.body.error, 'fixture_certificate_not_configured');
  assert.equal(readFileSync(fixture.journalPath, 'utf8'), '');
});

test('valid signed transaction retains exact string IDs and a provider-independent durable acknowledgement', async (t) => {
  const fixture = await runningFixture(t);
  const token = await fixture.request(tokenRoute, { headers: { Origin: 'tauri://localhost' } });
  assert.deepEqual(token.body, { app_account_token: FIXTURE_ACCOUNT_TOKEN });
  const response = await fixture.request(transactionRoute, {
    method: 'POST', headers: { Origin: 'tauri://localhost' }, body: { signed_transaction: signedTransaction() },
  });
  assert.equal(response.status, 200);
  assert.equal(response.headers.get('Access-Control-Allow-Origin'), 'tauri://localhost');
  assert.deepEqual(response.body, {
    acknowledged_transaction_id: '18446744073709551615', payment_provider: 'stripe', fixture_only: true,
  });
  // Reading after the acknowledgement sees the durable record; no JWS or token
  // is retained by the journal, status endpoint, or response.
  const journal = readFileSync(fixture.journalPath, 'utf8');
  assert.deepEqual(JSON.parse(journal), {
    transaction_id: '18446744073709551615', original_transaction_id: '9007199254740993',
    product_id: FIXTURE_PRODUCTS[0], environment: 'Xcode',
  });
  assert.ok(!journal.includes(FIXTURE_ACCOUNT_TOKEN));
  assert.ok(!journal.includes('signed_transaction'));
});

test('duplicate submissions are idempotent across restart and conflicts are rejected', async (t) => {
  const fixture = await runningFixture(t);
  const options = { method: 'POST', body: { signed_transaction: signedTransaction() } };
  assert.equal((await fixture.request(transactionRoute, options)).status, 200);
  assert.equal((await fixture.request(transactionRoute, options)).status, 200);
  await fixture.close();
  const restarted = await runningFixture(t, fixture.journalPath);
  assert.equal((await restarted.request(transactionRoute, options)).status, 200);
  const conflict = await restarted.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: signedTransaction({ productId: FIXTURE_PRODUCTS[1] }) },
  });
  assert.equal(conflict.status, 409);
  assert.equal(conflict.body.error, 'transaction_identity_conflict');
  assert.equal(readFileSync(fixture.journalPath, 'utf8').trim().split('\n').length, 1);
});

test('standalone verification requires a token unless an explicit lineage predicate accepts the exact original ID', () => {
  const transaction = signedTransaction({ appAccountToken: undefined });
  assert.throws(() => createTransactionVerifier(signer.certificateBytes)(transaction), /missing_account_token/);
  const verify = createTransactionVerifier(signer.certificateBytes, {
    isKnownOriginalTransaction: (id) => id === '9007199254740993',
  });
  assert.equal(verify(transaction).original_transaction_id, '9007199254740993');
  const nonBoolean = createTransactionVerifier(signer.certificateBytes, {
    isKnownOriginalTransaction: () => 'true',
  });
  assert.throws(() => nonBoolean(transaction), /missing_account_token/);
});

test('a durably acknowledged lineage permits tokenless product changes and survives restart', async (t) => {
  const fixture = await runningFixture(t);
  assert.equal((await fixture.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: signedTransaction() },
  })).status, 200);
  const upgrade = { method: 'POST', body: { signed_transaction: signedTransaction({
    transactionId: '18446744073709551616', productId: FIXTURE_PRODUCTS[1], appAccountToken: undefined,
  }) } };
  const upgraded = await fixture.request(transactionRoute, upgrade);
  assert.equal(upgraded.status, 200);
  assert.equal(upgraded.body.acknowledged_transaction_id, '18446744073709551616');
  assert.equal((await fixture.request(transactionRoute, upgrade)).status, 200);
  await fixture.close();
  const restarted = await runningFixture(t, fixture.journalPath);
  assert.equal((await restarted.request(transactionRoute, upgrade)).status, 200);
  assert.equal((await restarted.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: signedTransaction({
      transactionId: '18446744073709551617', productId: FIXTURE_PRODUCTS[2], appAccountToken: undefined,
    }) },
  })).status, 200);
  const records = readFileSync(fixture.journalPath, 'utf8').trim().split('\n').map(JSON.parse);
  assert.equal(records.length, 3);
  assert.deepEqual(records.map((record) => record.product_id), FIXTURE_PRODUCTS);
  assert.ok(records.every((record) => record.original_transaction_id === '9007199254740993'));
});

test('a known lineage never overrides a present wrong or invalid account token', async (t) => {
  const fixture = await runningFixture(t);
  assert.equal((await fixture.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: signedTransaction() },
  })).status, 200);
  const before = readFileSync(fixture.journalPath, 'utf8');
  for (const token of ['22222222-2222-4222-8222-222222222222', null, '', 42]) {
    const response = await fixture.request(transactionRoute, {
      method: 'POST', body: { signed_transaction: signedTransaction({
        transactionId: '18446744073709551616', appAccountToken: token,
      }) },
    });
    assert.equal(response.status, 409);
    assert.equal(response.body.error, 'wrong_account_token');
  }
  assert.equal(readFileSync(fixture.journalPath, 'utf8'), before);
});

test('tokenless transactions cannot first-claim an unknown original ID or infer it from a transaction ID', async (t) => {
  const fixture = await runningFixture(t);
  const tokenless = (originalTransactionId) => fixture.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: signedTransaction({
      transactionId: '43', originalTransactionId, appAccountToken: undefined,
    }) },
  });
  assert.equal((await tokenless('400')).status, 409);
  assert.equal(readFileSync(fixture.journalPath, 'utf8'), '');
  assert.equal((await fixture.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: signedTransaction({
      transactionId: '42', originalTransactionId: '400',
    }) },
  })).status, 200);
  const before = readFileSync(fixture.journalPath, 'utf8');
  for (const originalId of ['401', '42', '0400']) {
    const rejected = await tokenless(originalId);
    assert.equal(rejected.status, 409);
    assert.equal(rejected.body.error, 'missing_account_token');
  }
  assert.equal(readFileSync(fixture.journalPath, 'utf8'), before);
});

test('tokenless known-lineage transactions still reject conflicting transaction identity', async (t) => {
  const fixture = await runningFixture(t);
  assert.equal((await fixture.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: signedTransaction() },
  })).status, 200);
  const before = readFileSync(fixture.journalPath, 'utf8');
  const conflict = await fixture.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: signedTransaction({
      appAccountToken: undefined, productId: FIXTURE_PRODUCTS[1],
    }) },
  });
  assert.equal(conflict.status, 409);
  assert.equal(conflict.body.error, 'transaction_identity_conflict');
  assert.equal(readFileSync(fixture.journalPath, 'utf8'), before);
});

test('a failed append sync cannot be acknowledged until a restarted journal sync succeeds', async (t) => {
  const fixture = await runningFixture(t);
  const realFsync = fs.fsyncSync;
  let failSync = true;
  let syncAttempts = 0;
  let successfulSyncs = 0;
  const fsyncMock = mock.method(fs, 'fsyncSync', (fd) => {
    syncAttempts += 1;
    if (failSync) throw new Error('injected fixture fsync failure');
    realFsync(fd);
    successfulSyncs += 1;
  });
  syncBuiltinESMExports();
  try {
    const options = { method: 'POST', body: { signed_transaction: signedTransaction() } };
    const failed = await fixture.request(transactionRoute, options);
    assert.equal(failed.status, 503);
    assert.equal(failed.body.error, 'journal_unavailable');
    assert.equal(failed.body.acknowledged_transaction_id, undefined);
    assert.equal(syncAttempts, 1);
    assert.equal(successfulSyncs, 0);
    // The complete append exists, but the running process must not adopt it.
    assert.equal(readFileSync(fixture.journalPath, 'utf8').trim().split('\n').length, 1);
    const status = await fixture.request('/__test__/status', { headers: controlHeaders });
    assert.deepEqual(status.body.acknowledged_transactions, []);
    const tokenless = await fixture.request(transactionRoute, {
      method: 'POST', body: { signed_transaction: signedTransaction({
        transactionId: '18446744073709551616', appAccountToken: undefined,
      }) },
    });
    assert.equal(tokenless.status, 409);
    assert.equal(tokenless.body.error, 'missing_account_token');
    assert.equal((await fixture.request(transactionRoute, options)).status, 503);
    assert.equal(syncAttempts, 1);
    await fixture.close();

    assert.throws(() => createFixtureServer({ localFixture: true,
      certificateBytes: signer.certificateBytes, journalPath: fixture.journalPath }), /injected fixture fsync failure/);
    assert.equal(syncAttempts, 2);
    assert.equal(successfulSyncs, 0);

    failSync = false;
    const restarted = await runningFixture(t, fixture.journalPath);
    assert.equal(successfulSyncs, 2); // Startup syncs the journal and its directory.
    assert.equal((await restarted.request(transactionRoute, options)).status, 200);
    assert.equal(successfulSyncs, 2); // Duplicate reuses the now-synced record.
    assert.equal(readFileSync(fixture.journalPath, 'utf8').trim().split('\n').length, 1);
  } finally {
    fsyncMock.mock.restore();
    syncBuiltinESMExports();
  }
});

test('startup retries a failed parent-directory sync even when the journal already exists', async (t) => {
  const journalPath = join(output, 'parent-sync-retry.jsonl');
  const realFsync = fs.fsyncSync;
  let failDirectorySync = true;
  const syncAttempts = [];
  const fsyncMock = mock.method(fs, 'fsyncSync', (fd) => {
    const directory = fs.fstatSync(fd).isDirectory();
    syncAttempts.push(directory ? 'directory' : 'journal');
    if (directory && failDirectorySync) throw new Error('injected parent-directory fsync failure');
    realFsync(fd);
  });
  syncBuiltinESMExports();
  try {
    const start = () => createFixtureServer({ localFixture: true,
      certificateBytes: signer.certificateBytes, journalPath });
    assert.throws(start, /injected parent-directory fsync failure/);
    assert.equal(readFileSync(journalPath, 'utf8'), '');
    assert.deepEqual(syncAttempts, ['journal', 'directory']);
    // The failed first startup left the file behind; existence must not bypass
    // the directory sync when another startup attempts to serve requests.
    assert.throws(start, /injected parent-directory fsync failure/);
    assert.deepEqual(syncAttempts, ['journal', 'directory', 'journal', 'directory']);

    failDirectorySync = false;
    const fixture = await runningFixture(t, journalPath);
    assert.deepEqual(syncAttempts, ['journal', 'directory', 'journal', 'directory', 'journal', 'directory']);
    const response = await fixture.request(transactionRoute, {
      method: 'POST', body: { signed_transaction: signedTransaction() },
    });
    assert.equal(response.status, 200);
    assert.equal(response.body.acknowledged_transaction_id, '18446744073709551615');
    assert.equal(syncAttempts.at(-1), 'journal');
    assert.equal(readFileSync(journalPath, 'utf8').trim().split('\n').length, 1);
  } finally {
    fsyncMock.mock.restore();
    syncBuiltinESMExports();
  }
});

test('failure before acknowledgement writes nothing and recovery can acknowledge the retry', async (t) => {
  const fixture = await runningFixture(t);
  assert.equal((await fixture.request('/__test__/mode', {
    method: 'POST', headers: controlHeaders, body: { mode: 'unavailable' },
  })).status, 200);
  const options = { method: 'POST', body: { signed_transaction: signedTransaction() } };
  const failed = await fixture.request(transactionRoute, options);
  assert.equal(failed.status, 503);
  assert.equal(failed.body.acknowledged_transaction_id, undefined);
  assert.equal(readFileSync(fixture.journalPath, 'utf8'), '');
  await fixture.request('/__test__/mode', { method: 'POST', headers: controlHeaders, body: { mode: 'ok' } });
  assert.equal((await fixture.request(transactionRoute, options)).status, 200);
  const status = await fixture.request('/__test__/status', { headers: controlHeaders });
  assert.equal(status.body.acknowledged_transactions.length, 1);
});

test('tampered signature and an unpinned signer fail before journaling', async (t) => {
  const fixture = await runningFixture(t);
  const token = signedTransaction().split('.');
  const payload = JSON.parse(Buffer.from(token[1], 'base64url'));
  payload.transactionId = '1234';
  token[1] = Buffer.from(JSON.stringify(payload)).toString('base64url');
  for (const [jws, expected] of [[token.join('.'), 'invalid_jws_signature'],
    [signedTransaction({}, otherSigner), 'untrusted_test_certificate']]) {
    const response = await fixture.request(transactionRoute, { method: 'POST', body: { signed_transaction: jws } });
    assert.equal(response.status, 400);
    assert.equal(response.body.error, expected);
  }
  assert.equal(readFileSync(fixture.journalPath, 'utf8'), '');
});

test('host diagnostics expose only the last transaction rejection code and clear after acknowledgement', async (t) => {
  const fixture = await runningFixture(t);
  const status = () => fixture.request('/__test__/status', { headers: controlHeaders });
  assert.equal((await status()).body.last_rejection, null);
  const rejectedJws = signedTransaction({ productId: 'rejected.product.must.not.be.retained' });
  const rejected = await fixture.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: rejectedJws },
  });
  assert.equal(rejected.status, 400);
  assert.deepEqual(rejected.body, { error: 'wrong_product' });
  const diagnostic = (await status()).body;
  assert.deepEqual(diagnostic, { fixture_only: true, bootstrap_only: false, mode: 'ok',
    acknowledged_transactions: [], last_rejection: { status: 400, code: 'wrong_product' } });
  assert.ok(!JSON.stringify(diagnostic).includes(rejectedJws));
  assert.ok(!JSON.stringify(diagnostic).includes(FIXTURE_ACCOUNT_TOKEN));
  assert.ok(!JSON.stringify(diagnostic).includes('rejected.product.must.not.be.retained'));
  assert.equal((await fixture.request('/__test__/status', {
    headers: { ...controlHeaders, Origin: 'tauri://localhost' },
  })).status, 403);
  assert.deepEqual((await status()).body.last_rejection, { status: 400, code: 'wrong_product' });
  assert.equal((await fixture.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: signedTransaction() },
  })).status, 200);
  assert.equal((await status()).body.last_rejection, null);
});

for (const [name, changes, expected, status = 400] of [
  ['production environment', { environment: 'Production' }, 'wrong_environment'],
  ['sandbox environment', { environment: 'Sandbox' }, 'wrong_environment'],
  ['wrong bundle', { bundleId: 'cloud.example.other' }, 'wrong_bundle'],
  ['wrong product', { productId: 'other.product' }, 'wrong_product'],
  ['wrong account token', { appAccountToken: '22222222-2222-4222-8222-222222222222' }, 'wrong_account_token', 409],
  ['missing account token', { appAccountToken: undefined }, 'missing_account_token', 409],
  ['null account token', { appAccountToken: null }, 'wrong_account_token', 409],
  ['numeric ID', { transactionId: 9007199254740992 }, 'invalid_transaction_id'],
  ['missing original ID', { originalTransactionId: undefined }, 'invalid_transaction_id'],
]) {
  test(`rejects ${name} despite valid fixture signature`, async (t) => {
    const fixture = await runningFixture(t);
    const response = await fixture.request(transactionRoute, {
      method: 'POST', body: { signed_transaction: signedTransaction(changes) },
    });
    assert.equal(response.status, status);
    assert.equal(response.body.error, expected);
    assert.equal(readFileSync(fixture.journalPath, 'utf8'), '');
  });
}

test('rejects arbitrary browser origins, missing fixture header, and renderer control access', async (t) => {
  const fixture = await runningFixture(t);
  assert.equal((await fixture.request(tokenRoute, { headers: { Origin: 'https://example.com' } })).status, 403);
  assert.equal((await fixture.request(tokenRoute, { headers: { 'X-Maple-StoreKit-Fixture': '' } })).status, 403);
  const wrongHostStatus = await new Promise((resolve, reject) => {
    const request = httpRequest(`${fixture.base}${tokenRoute}`, {
      headers: { Host: 'example.com', 'X-Maple-StoreKit-Fixture': '1' },
    }, (response) => { response.resume(); resolve(response.statusCode); });
    request.once('error', reject);
    request.end();
  });
  assert.equal(wrongHostStatus, 403);
  assert.equal((await fixture.request('/__test__/mode', {
    method: 'POST', headers: { ...controlHeaders, Origin: 'tauri://localhost' }, body: { mode: 'unavailable' },
  })).status, 403);
  assert.equal((await fixture.request('/__test__/status')).status, 403);
});

test('permits only the intended Tauri preflight', async (t) => {
  const fixture = await runningFixture(t);
  const preflight = await fixture.request(transactionRoute, { method: 'OPTIONS', headers: {
    Origin: 'tauri://localhost', 'Access-Control-Request-Method': 'POST',
    'Access-Control-Request-Headers': 'content-type,x-maple-storekit-fixture',
  } });
  assert.equal(preflight.status, 204);
  assert.equal(preflight.headers.get('Access-Control-Allow-Origin'), 'tauri://localhost');
  assert.equal((await fixture.request(transactionRoute, { method: 'OPTIONS', headers: {
    Origin: 'tauri://localhost', 'Access-Control-Request-Method': 'POST',
    'Access-Control-Request-Headers': 'content-type,x-maple-storekit-fixture,x-maple-fixture-control',
  } })).status, 403);
});

test('bounds input and rejects unknown body fields', async (t) => {
  const fixture = await runningFixture(t);
  assert.equal((await fixture.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: 'x'.repeat(65536) },
  })).status, 413);
  assert.equal((await fixture.request(transactionRoute, {
    method: 'POST', body: { signed_transaction: signedTransaction(), skip_verification: true },
  })).status, 400);
});

test('rejects an incomplete journal rather than guessing whether it was acknowledged', () => {
  const journalPath = join(output, 'partial.jsonl');
  writeFileSync(journalPath, '{"transaction_id":');
  assert.throws(() => createFixtureServer({ localFixture: true, certificateBytes: signer.certificateBytes, journalPath }), /Incomplete fixture journal/);
});

test('rejects algorithm confusion before signature verification', () => {
  const verify = createTransactionVerifier(signer.certificateBytes);
  const parts = signedTransaction().split('.');
  const header = JSON.parse(Buffer.from(parts[0], 'base64url'));
  header.alg = 'none';
  parts[0] = Buffer.from(JSON.stringify(header)).toString('base64url');
  assert.throws(() => verify(parts.join('.')), /invalid_jws_algorithm/);
});
