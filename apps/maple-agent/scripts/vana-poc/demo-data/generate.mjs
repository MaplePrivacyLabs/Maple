// Entirely fictional fixtures. No account export, patient record, or real chat was used.
import { writeFile, mkdir } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const threads = [
  ['Synthetic demo persona: Alex Rivera', 'For this fictional demo, I am Alex Rivera, a privacy and AI engineer based in San Francisco. I am preparing an eight-minute talk in Washington, DC on October 15, 2026. None of these conversations or metrics belong to a real person.', 'Understood. I will treat Alex Rivera and every record in this demo as synthetic. Your talk can show useful personalization while being clear about where the data comes from.', 'My theme is: personal context should be permissioned, portable, and useful. Keep my answers concise and cite the vault records you use.', 'I will lead with the action, use short bullets, and identify the conversation or memory that supports each personalized recommendation.'],
  ['An eight-minute privacy talk', 'I have eight minutes for my DC privacy talk. How should I structure it?', 'Try four beats: a one-minute problem, a two-minute live vault demo, a three-minute explanation of permission boundaries, and a two-minute closing with questions.', 'I want one concrete outcome instead of a tour of technical features.', 'Show an assistant producing a personalized talk preparation plan from your own prior chats, with sources and an explicit grant.'],
  ['My working style', 'When helping me plan, I prefer a short prioritized list over a long essay. I want at most three priorities, each with a reason.', 'I will keep plans to three priorities, make the next action concrete, and put additional detail behind a follow-up.', 'Please cite the actual vault item. Do not invent studies or make guesses sound like stored facts.', 'I will distinguish retrieved facts from suggestions and cite the record title or scope for each retrieved fact.'],
  ['Portable context across assistants', 'My conversations are spread across tools. I want to carry my preferences into Maple without retyping a personal biography.', 'A personal vault can expose selected conversations and saved memories through MCP. Maple can read approved scopes and keep the resulting answer grounded in those records.', 'That is the part I want to demonstrate: my context is portable but I still choose the app and the scope.', 'Use a clear consent step and show the selected data categories before a read.'],
  ['MCP permissions for a native app', 'How should the permission screen for my personal vault feel?', 'Name the app, list the exact scopes, explain the purpose, and show whether the request is read-only. A user should be able to decline or revoke the grant.', 'For the first demo I only need ChatGPT conversations and saved memories. Health metrics should stay outside that grant.', 'Keep the first grant limited to chatgpt.conversations and chatgpt.memories. Add Oura only in a separate, deliberate request if the demo needs it.'],
  ['Maple demo success criteria', 'What would prove the Maple and Vana integration actually works?', 'A real MCP tool call should read a schema-valid block from the Personal Server, then Maple should answer using a fact available only in that block. A rejected ungranted scope is useful boundary evidence.', 'Avoid calling a screenshot or a mocked answer a completed integration.', 'Capture the tool call and the resulting answer separately, and label synthetic fixture content clearly.'],
  ['The question for the live demo', 'I need a question that makes context portability obvious.', 'Ask: Based on my previous chats, help me prepare my eight-minute DC privacy talk in two days. Give me three priorities and cite the relevant vault records.', 'Add my preferred speaking style and the local-data caveat.', 'The answer should reflect your preference for concrete examples, concise wording, and an honest statement that relay transport can still matter even when storage is local.'],
  ['Privacy language without overclaiming', 'Can I say all data stays on device if the MCP endpoint is a public HTTPS relay?', 'Be precise: local vault storage and network transport are separate claims. Describe the actual route and avoid promising that payloads never pass through a service until a direct local connection is verified.', 'I would rather say less than oversell this.', 'Use: Your vault is on your computer; you choose which context Maple may read. Then describe the demo transport accurately.'],
  ['A short opening story', 'Draft a simple opening for my talk. I dislike sweeping claims and buzzwords.', 'Every assistant starts by asking me who I am. But my past conversations already contain my preferences, projects, and deadlines. What if I could bring that context with me, and decide exactly what an app is allowed to see?', 'That is close. End with a concrete promise.', 'Today I will show one permissioned connection, one useful answer, and the records that answer came from.'],
  ['The single slide architecture', 'I only want one architecture slide.', 'Use three labeled boxes: Personal vault, approved MCP connection, Maple Agent. Put the permission decision at the connection and show context flowing into the answer.', 'Keep the labels understandable to a policy audience.', 'Call the scopes data categories on the slide, and show the precise scope names only in the product permission screen.'],
  ['A local endpoint roadmap', 'My future preference is a direct localhost MCP connection so a desktop user can choose a local transport.', 'Treat that as a separate product path to verify: endpoint binding, authorization, consent, metadata, token renewal, and revocation all need to work with the local origin.', 'Do not imply changing a URL is all it takes.', 'The demo can identify local transport as a next step while showing only the path actually tested.'],
  ['Preparing the screen recording', 'What should my product recording show?', 'Start with the vault contents, open Maple integrations, connect Vana with a clear permission choice, enable it for the chat, and ask a question that needs stored context.', 'I want a real product flow, not a montage of terminal commands.', 'Use clean framing and a few readable captions. Capture the actual app behavior and keep tokens or account keys out of the recording.'],
  ['Synthetic data disclosure', 'How do I make the demo compelling while being honest about fake data?', 'Use a cohesive fictional persona and a small visible Synthetic demo data label. The integration and tool calls can be real even when the imported records are fabricated.', 'Keep that label visible at the start and in the closing.', 'Then say: Synthetic history, live permissioned MCP reads. Avoid implying the fictional persona represents an actual account owner.'],
  ['Talk draft: three ideas', 'My three ideas are user choice, context portability, and useful personalization. Is that enough?', 'Yes. Map each idea to one action: choose scopes, bring prior chat context into a different assistant, and receive a preparation plan grounded in that context.', 'My policy audience does not need token exchange details.', 'Save protocol details for the appendix; use the live consent screen to make the boundary visible.'],
  ['Questions I expect from the audience', 'What questions should I prepare for in DC?', 'Expect: Can I revoke access? What leaves my device? Who operates the endpoint? Can an app ask for more data later? Is the answer based on real sources?', 'For anything untested, I want to say we are still verifying it.', 'Prepare a tested-versus-planned slide or note, and answer only with evidence from this build.'],
  ['Source citations in the final answer', 'What should citations look like when Maple uses my own chats?', 'Use the conversation title and scope, plus a record identifier when available. Make it clear that these are personal vault records rather than public research sources.', 'Please avoid citation spam after every sentence.', 'Group related claims and cite the supporting record once at the end of each short priority.'],
  ['Protecting my rehearsal time', 'My strongest focus is around 9 to 11 AM. I prefer meetings after lunch. Please keep that in mind when planning my rehearsal.', 'Reserve the morning for a full talk run-through and put logistics or review after lunch.', 'I want one rehearsal out loud, not another evening rewriting slides.', 'Put a complete spoken rehearsal before slide polishing, then set a stopping time for edits.'],
  ['Travel to Washington', 'For the fictional schedule, I travel to Washington on October 14 and speak on October 15. I want the demo and the talk outline ready before I leave.', 'Finish the core flow and a backup recording on October 13. Use October 14 for travel and one short rehearsal, then avoid making the live talk depend on a last-minute setup.', 'Include a fallback for unstable venue Wi-Fi.', 'Keep a local recording and screenshots of the tested flow, clearly identified as recorded evidence.'],
  ['A recovery-aware work plan', 'I have been staying up late to polish the demo. If I choose to share wearable data, I only want a descriptive summary that helps me schedule work, not a diagnosis.', 'A planning answer can summarize recorded sleep duration and your own stated preferences. It should avoid diagnoses, causal claims, or treatment advice.', 'Please do not read health scopes by default.', 'Use chat history first. Health records require a separate scope grant and a question that explicitly asks for them.'],
  ['Less late-night slide polishing', 'Late-night polishing is rarely useful for me. My own goal is to stop work by 10 PM on the two nights before the talk.', 'Put that goal in the preparation plan and make the final afternoon review small enough to finish before your stopping time.', 'That is a preference, not a medical rule.', 'I will present it as your stated working preference and avoid prescribing sleep targets from wearable data.'],
  ['Read-only means read-only', 'The integration in this demo should read vault records. It should not post messages or modify my source accounts.', 'Describe the demo as a read-only use of approved vault scopes. Check the tools available to the assistant and do not include external account actions in the recording.', 'I also want health excluded from the initial ChatGPT demo.', 'A denied Oura read under the chat-only grant can show that the boundary is real without sharing health data.'],
  ['Permission revocation wording', 'Draft a line that explains revocation without promising untested behavior.', 'You can choose the data categories this connection may read. Revocation is part of the permission model; the demo notes should identify which revocation behavior this build actually verifies.', 'Good. I do not want the narration to get ahead of the proof of concept.', 'Use the current tested grant checks as evidence and label further lifecycle checks as remaining work.'],
  ['What goes into the backup recording', 'My backup recording should be no more than two minutes.', 'Spend 20 seconds on the vault, 35 seconds on connection and permissions, 15 seconds enabling Vana in the chat, 40 seconds on a sourced answer, and 10 seconds on the closing disclosure.', 'Use a gentle pace and keep each label readable.', 'Leave a short pause when permissions and source citations appear so viewers can read them.'],
  ['Final talk title', 'Pick a grounded title for the DC privacy talk.', 'Your context, your choice: personal vaults meet useful AI.', 'I like that. Do not use a futuristic title.', 'Keep this title and use the live preparation plan as the concrete example under it.'],
  ['Final demo priorities for October 13', 'It is October 13. My talk is in two days. What matters most today?', 'Finish the live chat-only grant and sourced Maple answer; capture a readable backup recording; do one eight-minute rehearsal out loud. Leave cosmetic polish until those three work.', 'Use my preference for a short prioritized answer when I ask Maple later.', 'I will give three priorities with clear reasons and citations to the relevant saved conversations.'],
  ['A health-aware variant, only after permission', 'For an optional second demo, I might explicitly grant Oura sleep and readiness and ask for a descriptive summary alongside my talk schedule.', 'Use the exact approved scopes, report the recorded date range and averages, and state that synthetic wearable records support planning rather than medical conclusions.', 'Keep the main video centered on portable ChatGPT context. The health variant is secondary.', 'The main story stays focused on your prior chats and memories, with Oura available only as an optional separately granted example.'],
];

const memoryText = [
  'SYNTHETIC DEMO PERSONA: Alex Rivera is a fictional privacy and AI engineer. Every conversation, preference, and wearable metric in this vault fixture is fabricated for the Maple + Vana proof of concept.',
  'Alex Rivera is preparing an eight-minute Washington, DC privacy talk on October 15, 2026, titled Your context, your choice: personal vaults meet useful AI.',
  'Alex prefers concise answers with at most three priorities, concrete next actions, and citations to the exact personal vault records used.',
  'Alex prefers plain language, concrete examples, and an honest description of tested behavior over sweeping privacy claims or buzzwords.',
  'The talk theme is personal context that is permissioned, portable, and useful. The live example is a preparation plan grounded in prior ChatGPT conversations.',
  'Alex wants the initial Maple grant to include only chatgpt.conversations and chatgpt.memories. Oura sleep and readiness require a separate explicit permission choice.',
  'Alex has their best focus from 9 to 11 AM and prefers meetings and logistics after lunch. A spoken rehearsal comes before further slide polishing.',
  'Alex travels to Washington on October 14 and wants the demo, talk outline, and backup recording ready on October 13.',
  'Alex wants to stop work by 10 PM on the two nights before the talk. This is a personal scheduling preference, not a medical recommendation.',
  'Alex wants one simple architecture slide and a backup recording no longer than two minutes in case venue Wi-Fi is unreliable.',
  'Alex wants source citations to distinguish private vault conversation records from public research references. Retrieved facts and suggestions should be separate.',
  'All Oura fixtures are synthetic. Alex only wants descriptive date ranges and metric summaries if health scopes are deliberately granted; no diagnoses or treatment advice.',
];

export function buildFixtures() {
  const start = Date.parse('2026-09-18T16:00:00.000Z');
  const conversations = threads.map(([title, ...text], index) => {
    const created = start + index * 86400000;
    const id = `synthetic-alex-chat-${String(index + 1).padStart(2, '0')}`;
    const messages = text.map((content, messageIndex) => ({
      id: `${id}-message-${messageIndex + 1}`,
      role: messageIndex % 2 === 0 ? 'user' : 'assistant',
      content,
      content_type: 'text',
      create_time: new Date(created + messageIndex * 180000).toISOString(),
      model: messageIndex % 2 === 0 ? null : 'synthetic-demo-assistant',
    }));
    return { id, title, create_time: messages[0].create_time, update_time: messages.at(-1).create_time, message_count: messages.length, messages };
  });
  const memories = memoryText.map((content, index) => ({
    id: `synthetic-alex-memory-${String(index + 1).padStart(2, '0')}`,
    content,
    created_at: new Date(start + index * 86400000).toISOString(),
    updated_at: '2026-10-13T16:30:00.000Z',
    type: 'memory',
  }));
  // 14 nights ending Oct 13; a gentle fictional late-work pattern, no clinical claims.
  const minutes = [470, 455, 462, 448, 475, 460, 450, 440, 425, 410, 402, 395, 415, 430];
  const sleepScores = [87, 85, 86, 83, 89, 86, 84, 82, 79, 77, 75, 74, 78, 81];
  const readinessScores = [86, 84, 85, 82, 88, 86, 83, 81, 79, 77, 76, 75, 78, 80];
  const dayStart = Date.parse('2026-09-30T00:00:00.000Z');
  const dailyScores = [];
  const sleepPeriods = [];
  const days = [];
  minutes.forEach((durationMinutes, index) => {
    const day = new Date(dayStart + index * 86400000).toISOString().slice(0, 10);
    const total = durationMinutes * 60;
    const awake = [30, 32, 28, 35, 25, 29, 31, 34, 36, 40, 41, 42, 37, 33][index] * 60;
    const bedEnd = Date.parse(`${day}T14:15:00.000Z`); // 07:15 PDT
    const bedtimeStart = new Date(bedEnd - (total + awake) * 1000).toISOString();
    const bedtimeEnd = new Date(bedEnd).toISOString();
    const deep = Math.round(total * 0.18 / 60) * 60;
    const rem = Math.round(total * 0.24 / 60) * 60;
    const light = total - deep - rem;
    const score = sleepScores[index];
    dailyScores.push({ id: `synthetic-oura-sleep-score-${day}`, day, score, timestamp: bedtimeEnd, contributors: {
      deep_sleep: score - 1, efficiency: Math.round(total / (total + awake) * 100), latency: 87, rem_sleep: score + 2,
      restfulness: score - 3, timing: score - 2, total_sleep: score,
    } });
    sleepPeriods.push({ id: `synthetic-oura-sleep-period-${day}`, day, type: 'long_sleep', bedtimeStart, bedtimeEnd,
      totalSleepDuration: total, timeInBed: total + awake, deepSleepDuration: deep, lightSleepDuration: light,
      remSleepDuration: rem, awakeTime: awake, efficiency: Math.round(total / (total + awake) * 100), latency: 720,
      averageHeartRate: index > 7 ? 59.2 : 56.8, averageHrv: index > 7 ? 43 : 51, lowestHeartRate: index > 7 ? 52 : 49,
      averageBreath: 14.1, restlessPeriods: index > 7 ? 9 : 6,
    });
    const ready = readinessScores[index];
    days.push({ id: `synthetic-oura-readiness-${day}`, day, score: ready, timestamp: bedtimeEnd,
      temperatureDeviation: 0.0, temperatureTrendDeviation: 0.0, contributors: {
        activity_balance: 85, body_temperature: 95, hrv_balance: ready - 2, previous_day_activity: 86,
        previous_night: score, recovery_index: ready - 1, resting_heart_rate: ready + 1, sleep_balance: ready,
      },
    });
  });
  return {
    'chatgpt.conversations': { conversations, total: conversations.length },
    'chatgpt.memories': { memories, total: memories.length },
    'oura.sleep': { dailyScores, sleepPeriods },
    'oura.readiness': { days },
  };
}

export function buildExportEnvelopes(fixtures) {
  const timestamp = '2026-10-13T18:30:00.000Z';
  const c = fixtures['chatgpt.conversations'];
  const m = fixtures['chatgpt.memories'];
  const s = fixtures['oura.sleep'];
  const r = fixtures['oura.readiness'];
  return {
    'chatgpt-export': { requestedScopes: ['chatgpt.conversations', 'chatgpt.memories'], timestamp, version: '4.0.0-playwright', platform: 'chatgpt',
      exportSummary: { count: c.total, label: 'conversations', details: { synthetic: true, persona: 'Alex Rivera (fictional)', conversations: c.total, memories: m.total, messages: c.conversations.reduce((sum, item) => sum + item.message_count, 0) } },
      errors: [], 'chatgpt.conversations': c, 'chatgpt.memories': m },
    'oura-export': { requestedScopes: ['oura.sleep', 'oura.readiness'], timestamp, version: '3.1.2', platform: 'oura',
      exportSummary: { count: r.days.length + s.dailyScores.length + s.sleepPeriods.length, label: 'days of Oura data', details: { synthetic: true, persona: 'Alex Rivera (fictional)', readiness: r.days.length, sleepScores: s.dailyScores.length, sleepPeriods: s.sleepPeriods.length, activity: 0 } },
      errors: [], 'oura.sleep': s, 'oura.readiness': r },
  };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const destination = dirname(fileURLToPath(import.meta.url));
  await mkdir(destination, { recursive: true });
  const fixtures = buildFixtures();
  for (const [name, data] of Object.entries({ ...fixtures, ...buildExportEnvelopes(fixtures) })) {
    await writeFile(join(destination, `${name}.json`), `${JSON.stringify(data, null, 2)}\n`);
  }
  process.stdout.write('Generated synthetic fixtures: 26 chats, 12 memories, 14 days of Oura metrics.\n');
}
