import test from 'node:test';
import assert from 'node:assert/strict';
import { buildFixtures, buildExportEnvelopes } from './generate.mjs';
import { compileValidators, loadAndValidateFixtures, validateConsistency } from './validate.mjs';

test('all saved fixtures validate against installed Vana schemas and coherent dates/counts/durations', async () => {
  const fixtures = await loadAndValidateFixtures();
  assert.deepEqual(fixtures, buildFixtures());
});
test('ChatGPT schemas reject missing message content and extra fields', async () => {
  const validate = await compileValidators();
  const fixtures = buildFixtures();
  const chats = structuredClone(fixtures['chatgpt.conversations']);
  delete chats.conversations[0].messages[0].content;
  assert.equal(validate['chatgpt.conversations'](chats), false);
  const memories = structuredClone(fixtures['chatgpt.memories']);
  memories.memories[0].privateKey = 'this-field-must-not-be-allowed';
  assert.equal(validate['chatgpt.memories'](memories), false);
});
test('Oura schemas reject invalid dates, out of range scores, and invalid sleep types', async () => {
  const validate = await compileValidators(); const fixtures = buildFixtures();
  const readiness = structuredClone(fixtures['oura.readiness']);
  readiness.days[0].score = 101; assert.equal(validate['oura.readiness'](readiness), false);
  readiness.days[0].score = 80; readiness.days[0].day = '2026-02-31'; assert.equal(validate['oura.readiness'](readiness), false);
  const sleep = structuredClone(fixtures['oura.sleep']); sleep.sleepPeriods[0].type = 'invented';
  assert.equal(validate['oura.sleep'](sleep), false);
});
test('consistency validation catches plausible-looking corrupted record counts and stage totals', () => {
  const fixtures = buildFixtures(); fixtures['chatgpt.conversations'].conversations[0].message_count = 6;
  assert.throws(() => validateConsistency(fixtures));
  const other = buildFixtures(); other['oura.sleep'].sleepPeriods[0].deepSleepDuration += 60;
  assert.throws(() => validateConsistency(other));
});
test('optional native export envelopes contain canonical metadata and only declared scopes', () => {
  const fixtures = buildFixtures(); const envelopes = buildExportEnvelopes(fixtures);
  for (const envelope of Object.values(envelopes)) {
    assert.deepEqual(Object.keys(envelope).sort(), ['requestedScopes', 'timestamp', 'version', 'platform', 'exportSummary', 'errors', ...envelope.requestedScopes].sort());
    assert.equal(envelope.exportSummary.details.synthetic, true);
    for (const scope of envelope.requestedScopes) assert.deepEqual(envelope[scope], fixtures[scope]);
  }
});
