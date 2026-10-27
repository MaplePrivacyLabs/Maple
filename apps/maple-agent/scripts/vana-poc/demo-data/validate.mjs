// Offline validation against the actual installed Vana connector schemas.
import assert from 'node:assert/strict';
import { readFile, lstat } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
const bundle = '/Applications/Vana.app/Contents/Resources/personal-server/dist/node_modules';
const { default: Ajv } = await import(pathToFileURL(`${bundle}/ajv/dist/ajv.js`));
const { default: addFormats } = await import(pathToFileURL(`${bundle}/ajv-formats/dist/index.js`));
const dataDirectory = dirname(fileURLToPath(import.meta.url));
export const scopes = Object.freeze(['chatgpt.conversations', 'chatgpt.memories', 'oura.sleep', 'oura.readiness']);
export const schemaFiles = Object.freeze({
  'chatgpt.conversations': 'openai/chatgpt.conversations.json',
  'chatgpt.memories': 'openai/chatgpt.memories.json',
  'oura.sleep': 'oura/oura.sleep.json',
  'oura.readiness': 'oura/oura.readiness.json',
});
export async function compileValidators() {
  const ajv = new Ajv({ strict: false, allErrors: true });
  addFormats(ajv);
  const validators = {};
  for (const scope of scopes) {
    const wrapper = JSON.parse(await readFile(join('/Applications/Vana.app/Contents/Resources/connectors', schemaFiles[scope]), 'utf8'));
    assert.equal(wrapper.scope, scope);
    assert.equal(wrapper.version, scope.startsWith('chatgpt.') ? '2.0.0' : '1.0.0');
    validators[scope] = ajv.compile(wrapper.schema);
  }
  return validators;
}
export function validateConsistency(fixtures) {
  const { conversations, total } = fixtures['chatgpt.conversations'];
  assert.equal(total, 26);
  assert.equal(total, conversations.length);
  const ids = new Set();
  for (const chat of conversations) {
    assert.equal(chat.message_count, chat.messages.length);
    assert.equal(chat.messages.length, 4);
    assert.ok(Date.parse(chat.create_time) <= Date.parse(chat.update_time));
    assert.ok(chat.id.startsWith('synthetic-'));
    assert.ok(!ids.has(chat.id)); ids.add(chat.id);
    chat.messages.forEach((message, index) => {
      assert.equal(message.role, index % 2 === 0 ? 'user' : 'assistant');
      assert.ok(!ids.has(message.id)); ids.add(message.id);
      assert.ok(Date.parse(message.create_time) >= Date.parse(chat.create_time));
      assert.ok(Date.parse(message.create_time) <= Date.parse(chat.update_time));
    });
  }
  assert.match(conversations[0].messages[0].content, /fictional demo/);
  const memory = fixtures['chatgpt.memories'];
  assert.equal(memory.total, 12); assert.equal(memory.total, memory.memories.length);
  assert.match(memory.memories[0].content, /SYNTHETIC DEMO PERSONA/);
  assert.ok(memory.memories.every(m => m.id.startsWith('synthetic-') && Date.parse(m.created_at) <= Date.parse(m.updated_at)));
  const sleep = fixtures['oura.sleep']; const readiness = fixtures['oura.readiness'];
  assert.equal(sleep.dailyScores.length, 14); assert.equal(sleep.sleepPeriods.length, 14); assert.equal(readiness.days.length, 14);
  sleep.sleepPeriods.forEach((period, index) => {
    assert.equal(period.day, sleep.dailyScores[index].day);
    assert.equal(period.day, readiness.days[index].day);
    assert.ok(period.id.startsWith('synthetic-'));
    assert.equal(period.deepSleepDuration + period.lightSleepDuration + period.remSleepDuration, period.totalSleepDuration);
    assert.equal(period.totalSleepDuration + period.awakeTime, period.timeInBed);
    assert.equal((Date.parse(period.bedtimeEnd) - Date.parse(period.bedtimeStart)) / 1000, period.timeInBed);
    assert.equal(period.efficiency, Math.round(period.totalSleepDuration / period.timeInBed * 100));
    assert.ok(period.totalSleepDuration >= 6 * 3600 && period.totalSleepDuration <= 9 * 3600);
    if (index > 0) assert.equal(Date.parse(`${period.day}T00:00:00Z`) - Date.parse(`${sleep.sleepPeriods[index - 1].day}T00:00:00Z`), 86400000);
  });
  assert.equal(sleep.sleepPeriods[0].day, '2026-09-30');
  assert.equal(sleep.sleepPeriods.at(-1).day, '2026-10-13');
}
export async function loadAndValidateFixtures() {
  const validators = await compileValidators(); const fixtures = {};
  for (const scope of scopes) {
    const path = join(dataDirectory, `${scope}.json`);
    const stat = await lstat(path);
    assert.ok(stat.isFile() && !stat.isSymbolicLink() && stat.size < 262144);
    const item = JSON.parse(await readFile(path, 'utf8'));
    assert.ok(validators[scope](item), `Schema validation failed: ${scope}`);
    fixtures[scope] = item;
  }
  validateConsistency(fixtures);
  return fixtures;
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await loadAndValidateFixtures();
  process.stdout.write('Installed Vana schemas and cross-record consistency passed for all four scopes.\n');
}
