#!/usr/bin/env node
// Local StoreKit harness only. This is not a billing or entitlement server.
import { X509Certificate, verify as verifySignature } from 'node:crypto';
import {
  closeSync, constants, fstatSync, fsyncSync, openSync,
  readFileSync, writeSync,
} from 'node:fs';
import { createServer } from 'node:http';
import { dirname, isAbsolute, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

export const FIXTURE_PORT = 38863;
export const FIXTURE_HEADER = '1';
export const FIXTURE_ACCOUNT_TOKEN = '11111111-1111-4111-8111-111111111111';
export const FIXTURE_BUNDLE_ID = 'cloud.opensecret.maple';
export const FIXTURE_PRODUCTS = Object.freeze([
  'cloud.opensecret.maple.pro.monthly',
  'cloud.opensecret.maple.max.monthly',
  'cloud.opensecret.maple.pro.yearly',
]);
const ALLOWED_ORIGINS = new Set(['tauri://localhost', 'http://tauri.localhost']);
const MAX_BODY_BYTES = 64 * 1024;

class FixtureError extends Error {
  constructor(code, status = 400) {
    super(code);
    this.code = code;
    this.status = status;
  }
}

function rejectUnless(condition, code, status) {
  if (!condition) throw new FixtureError(code, status);
}

function decodeBase64Url(value) {
  rejectUnless(typeof value === 'string' && /^[A-Za-z0-9_-]+$/.test(value), 'invalid_jws');
  const decoded = Buffer.from(value, 'base64url');
  rejectUnless(decoded.toString('base64url') === value, 'invalid_jws');
  return decoded;
}

function decodeJson(value) {
  try {
    const object = JSON.parse(decodeBase64Url(value).toString('utf8'));
    rejectUnless(object !== null && !Array.isArray(object) && typeof object === 'object', 'invalid_jws');
    return object;
  } catch {
    throw new FixtureError('invalid_jws');
  }
}

function transactionId(value) {
  // Never coerce JSON numbers: real transaction identifiers can exceed 2^53.
  rejectUnless(typeof value === 'string' && /^[0-9]{1,64}$/.test(value), 'invalid_transaction_id');
  return value;
}

export function createTransactionVerifier(certificateBytes, { isKnownOriginalTransaction = () => false } = {}) {
  if (typeof isKnownOriginalTransaction !== 'function') throw new Error('Expected an explicit known-lineage predicate.');
  const pinned = new X509Certificate(certificateBytes);
  if (pinned.publicKey.asymmetricKeyType !== 'ec'
      || pinned.publicKey.asymmetricKeyDetails?.namedCurve !== 'prime256v1') {
    throw new Error('Pin the ES256 StoreKit 2 test signing certificate; the RSA StoreKitTestCertificate.cer is for receipts.');
  }
  // StoreKit testing uses a single self-signed certificate. The operator pins
  // its public certificate from the native-verified local test run; a request
  // can never supply or replace the trust anchor.
  if (!pinned.verify(pinned.publicKey)) throw new Error('Expected a self-signed local StoreKit 2 test certificate.');
  return (jws) => {
    rejectUnless(typeof jws === 'string' && Buffer.byteLength(jws) <= MAX_BODY_BYTES, 'invalid_jws');
    const pieces = jws.split('.');
    rejectUnless(pieces.length === 3, 'invalid_jws');
    const [encodedHeader, encodedPayload, encodedSignature] = pieces;
    const header = decodeJson(encodedHeader);
    rejectUnless(header.alg === 'ES256' && header.crit === undefined && header.b64 === undefined, 'invalid_jws_algorithm');
    rejectUnless(Array.isArray(header.x5c) && header.x5c.length === 1
      && typeof header.x5c[0] === 'string', 'invalid_test_certificate');
    const suppliedCertificate = Buffer.from(header.x5c[0], 'base64');
    rejectUnless(suppliedCertificate.toString('base64') === header.x5c[0]
      && suppliedCertificate.equals(pinned.raw), 'untrusted_test_certificate');
    const now = Date.now();
    rejectUnless(now >= Date.parse(pinned.validFrom) && now <= Date.parse(pinned.validTo), 'expired_test_certificate');
    const signature = decodeBase64Url(encodedSignature);
    rejectUnless(signature.length === 64 && verifySignature('sha256',
      Buffer.from(`${encodedHeader}.${encodedPayload}`),
      { key: pinned.publicKey, dsaEncoding: 'ieee-p1363' }, signature), 'invalid_jws_signature');
    const payload = decodeJson(encodedPayload);
    rejectUnless(payload.environment === 'Xcode', 'wrong_environment');
    rejectUnless(payload.bundleId === FIXTURE_BUNDLE_ID, 'wrong_bundle');
    rejectUnless(FIXTURE_PRODUCTS.includes(payload.productId), 'wrong_product');
    const id = transactionId(payload.transactionId);
    const originalId = transactionId(payload.originalTransactionId);
    if (payload.appAccountToken === undefined) {
      // StoreKit management can omit the token on a later transaction. Only
      // an already acknowledged exact original ID can establish its ownership.
      rejectUnless(isKnownOriginalTransaction(originalId) === true, 'missing_account_token', 409);
    } else {
      rejectUnless(typeof payload.appAccountToken === 'string'
        && payload.appAccountToken.toLowerCase() === FIXTURE_ACCOUNT_TOKEN, 'wrong_account_token', 409);
    }
    return {
      transaction_id: id,
      original_transaction_id: originalId,
      product_id: payload.productId,
      environment: 'Xcode',
    };
  };
}

class AcknowledgementJournal {
  constructor(path) {
    if (!isAbsolute(path)) throw new Error('The fixture journal path must be absolute.');
    this.fd = openSync(path, constants.O_RDWR | constants.O_APPEND | constants.O_CREAT | constants.O_NOFOLLOW, 0o600);
    try {
      if (!fstatSync(this.fd).isFile()) throw new Error('The fixture journal must be a regular file.');
      const previous = readFileSync(this.fd, 'utf8');
      if (previous && !previous.endsWith('\n')) throw new Error('Incomplete fixture journal; preserve it and start a new run.');
      this.records = new Map();
      for (const line of previous.split('\n').filter(Boolean)) {
        const record = JSON.parse(line);
        transactionId(record.transaction_id);
        transactionId(record.original_transaction_id);
        if (record.environment !== 'Xcode' || !FIXTURE_PRODUCTS.includes(record.product_id)
          || Object.keys(record).sort().join(',') !== 'environment,original_transaction_id,product_id,transaction_id'
          || this.records.has(record.transaction_id)) throw new Error('Invalid fixture journal.');
        this.records.set(record.transaction_id, record);
      }
      // A complete record may remain after an earlier append succeeded but
      // its fsync failed. Reestablish durability before accepting duplicates.
      fsyncSync(this.fd);
      // An earlier startup may have created this file but failed to sync its
      // directory entry. File existence alone does not establish durability.
      const parent = openSync(dirname(path), constants.O_RDONLY);
      try { fsyncSync(parent); } finally { closeSync(parent); }
      this.failed = false;
    } catch (error) {
      closeSync(this.fd);
      throw error;
    }
  }

  hasOriginalTransaction(originalId) {
    for (const record of this.records.values()) {
      if (record.original_transaction_id === originalId) return true;
    }
    return false;
  }

  acknowledge(record) {
    rejectUnless(!this.failed, 'journal_unavailable', 503);
    const previous = this.records.get(record.transaction_id);
    if (previous) {
      rejectUnless(previous.original_transaction_id === record.original_transaction_id
        && previous.product_id === record.product_id, 'transaction_identity_conflict', 409);
      return;
    }
    try {
      const line = Buffer.from(`${JSON.stringify(record)}\n`);
      let written = 0;
      while (written < line.length) written += writeSync(this.fd, line, written, line.length - written);
      fsyncSync(this.fd); // The HTTP acknowledgment is emitted only after this succeeds.
      this.records.set(record.transaction_id, record);
    } catch {
      this.failed = true;
      throw new FixtureError('journal_unavailable', 503);
    }
  }

  close() { closeSync(this.fd); }
}

async function readJson(request) {
  rejectUnless(request.headers['content-type']?.split(';')[0].trim() === 'application/json', 'expected_json', 415);
  const declared = request.headers['content-length'];
  if (declared !== undefined) rejectUnless(/^\d+$/.test(declared)
    && Number(declared) <= MAX_BODY_BYTES, 'body_too_large', 413);
  const chunks = [];
  let size = 0;
  for await (const chunk of request) {
    size += chunk.length;
    rejectUnless(size <= MAX_BODY_BYTES, 'body_too_large', 413);
    chunks.push(chunk);
  }
  try {
    const parsed = JSON.parse(Buffer.concat(chunks).toString('utf8'));
    rejectUnless(parsed !== null && !Array.isArray(parsed) && typeof parsed === 'object', 'invalid_json');
    return parsed;
  } catch {
    throw new FixtureError('invalid_json');
  }
}

export function createFixtureServer({ certificateBytes, journalPath, localFixture = false, bootstrapOnly = false }) {
  if (localFixture !== true) throw new Error('Refusing startup without explicit local-fixture mode.');
  if (bootstrapOnly && certificateBytes !== undefined) throw new Error('Bootstrap mode cannot configure a signing certificate.');
  // The predicate runs only when handling a verified request, after journal
  // construction has validated and synced all restored acknowledgements.
  const verify = bootstrapOnly ? undefined : createTransactionVerifier(certificateBytes, {
    isKnownOriginalTransaction: (originalId) => journal.hasOriginalTransaction(originalId),
  });
  const journal = new AcknowledgementJournal(journalPath);
  let mode = 'ok';
  let lastRejection = null;
  const server = createServer(async (request, response) => {
    const send = (status, body) => {
      response.writeHead(status, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' });
      response.end(JSON.stringify(body));
    };
    try {
      const origin = request.headers.origin;
      rejectUnless(origin === undefined || ALLOWED_ORIGINS.has(origin), 'origin_not_allowed', 403);
      rejectUnless(request.headers.host === `127.0.0.1:${server.address()?.port}`, 'host_not_allowed', 403);
      if (origin) {
        response.setHeader('Access-Control-Allow-Origin', origin);
        response.setHeader('Vary', 'Origin');
      }
      if (request.method === 'OPTIONS') {
        rejectUnless(origin !== undefined, 'origin_required', 403);
        rejectUnless(['GET', 'POST'].includes(request.headers['access-control-request-method']), 'method_not_allowed', 405);
        const requestedHeaders = (request.headers['access-control-request-headers'] ?? '').toLowerCase().split(',').map((item) => item.trim());
        rejectUnless(requestedHeaders.includes('x-maple-storekit-fixture')
          && requestedHeaders.every((item) => ['content-type', 'x-maple-storekit-fixture'].includes(item)), 'headers_not_allowed', 403);
        response.setHeader('Access-Control-Allow-Methods', 'GET, POST');
        response.setHeader('Access-Control-Allow-Headers', 'Content-Type, X-Maple-StoreKit-Fixture');
        response.writeHead(204);
        response.end();
        return;
      }
      rejectUnless(request.headers['x-maple-storekit-fixture'] === FIXTURE_HEADER, 'fixture_header_required', 403);
      if (request.url?.startsWith('/__test__/')) {
        // Test controls are available to the host harness, never the renderer.
        rejectUnless(origin === undefined
          && request.headers['x-maple-fixture-control'] === 'storekit-harness', 'harness_control_required', 403);
        if (request.method === 'POST' && request.url === '/__test__/mode') {
          const body = await readJson(request);
          rejectUnless(Object.keys(body).length === 1 && ['ok', 'unavailable'].includes(body.mode), 'invalid_mode');
          mode = body.mode;
          send(200, { mode });
          return;
        }
        if (request.method === 'GET' && request.url === '/__test__/status') {
          send(200, { fixture_only: true, bootstrap_only: bootstrapOnly, mode,
            acknowledged_transactions: [...journal.records.values()], last_rejection: lastRejection });
          return;
        }
      }
      if (request.method === 'GET' && request.url === '/v1/maple/subscription/apple/account-token') {
        send(200, { app_account_token: FIXTURE_ACCOUNT_TOKEN });
        return;
      }
      if (request.method === 'POST' && request.url === '/v1/maple/subscription/apple/transactions') {
        rejectUnless(verify !== undefined, 'fixture_certificate_not_configured', 503);
        rejectUnless(mode === 'ok', 'fixture_temporarily_unavailable', 503);
        const body = await readJson(request);
        rejectUnless(mode === 'ok', 'fixture_temporarily_unavailable', 503);
        rejectUnless(Object.keys(body).length === 1 && typeof body.signed_transaction === 'string', 'invalid_transaction_request');
        const record = verify(body.signed_transaction);
        journal.acknowledge(record);
        lastRejection = null;
        send(200, { acknowledged_transaction_id: record.transaction_id, payment_provider: 'stripe', fixture_only: true });
        return;
      }
      send(404, { error: 'not_found' });
    } catch (error) {
      const status = error instanceof FixtureError ? error.status : 500;
      const code = error instanceof FixtureError ? error.code : 'fixture_internal_error';
      if (request.method === 'POST' && request.url === '/v1/maple/subscription/apple/transactions') {
        // Only fixed fixture error codes reach host diagnostics, never error
        // messages, request bodies, signed transactions, tokens, or headers.
        lastRejection = { status, code };
      }
      send(status, { error: code });
    }
  });
  server.requestTimeout = 10_000;
  server.headersTimeout = 10_000;
  server.maxHeadersCount = 32;
  server.once('close', () => journal.close());
  return server;
}

async function main() {
  const args = process.argv.slice(2);
  if (!args.includes('--local-fixture')) throw new Error('Refusing startup: --local-fixture is required.');
  const options = {};
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === '--local-fixture') continue;
    if (args[index] === '--bootstrap-only') { options.bootstrapOnly = true; continue; }
    const option = args[index];
    if (!['--certificate', '--journal'].includes(option) || options[option] || !args[index + 1]) {
      throw new Error('Usage: fixture-server.mjs --local-fixture (--bootstrap-only | --certificate <public-test-certificate>) --journal <absolute-existing-output-directory>/acknowledgements.jsonl');
    }
    options[option] = args[++index];
  }
  if (!options['--journal'] || (!options.bootstrapOnly && !options['--certificate'])) throw new Error('--journal and either --certificate or --bootstrap-only are required.');
  if (options.bootstrapOnly && options['--certificate']) throw new Error('--bootstrap-only and --certificate are mutually exclusive.');
  const server = createFixtureServer({ localFixture: true,
    bootstrapOnly: options.bootstrapOnly === true,
    certificateBytes: options['--certificate'] ? readFileSync(options['--certificate']) : undefined,
    journalPath: options['--journal'] });
  await new Promise((resolveListen, rejectListen) => {
    server.once('error', rejectListen);
    server.listen(FIXTURE_PORT, '127.0.0.1', resolveListen);
  });
  console.log(`Local StoreKit fixture only: http://127.0.0.1:${FIXTURE_PORT}; bootstrap=${options.bootstrapOnly === true}; no real entitlement grants.`);
  for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => server.close());
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch((error) => {
    // Startup errors carry configuration descriptions, never request bodies.
    console.error(error.message);
    process.exitCode = 1;
  });
}
