// Narrow scenario-specific volatile value contract. Never mutates raw captures.
// Fixed baseline BlueBubblesChannel.ts:71 initializes floor(Date.now()/1000)-5;
// :121-126 only advances on a newer row. This fixture's mocked dates 1/2 cannot
// advance it. Exact -5 behavior remains covered by channel oracle/tests.
import { isDeepStrictEqual } from 'node:util';

const SCENARIO = 'backend-core-channel-recovery';
const SESSION = 'bluebubbles:chat=core-chat:general';
const DECLARATION = { policy: 'coreChannelPollAfter' };
const PHASES = [
  { name: 'initial', count: 4, waitMs: 8000, text: 'CORE-BEGIN-{saved.channelText}' },
  { name: 'recovered', count: 3, waitMs: 5000, text: 'CORE-BEGIN-CORE-RECOVER' },
];
const insist = (ok, why) => { if (!ok) throw Error(`channel poll policy: ${why}`); };
const stamp = (value) => {
  insist(typeof value === 'string' && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(value), 'durable input timestamp must be an ISO millisecond value');
  const ms = Date.parse(value);
  insist(Number.isSafeInteger(ms) && new Date(ms).toISOString() === value, 'durable input timestamp is invalid');
  return ms;
};
function inputText(entry) {
  insist(Array.isArray(entry.messages) && entry.messages.length === 1, 'durable input must have one user message');
  const message = entry.messages[0];
  insist(message?.role === 'user' && Array.isArray(message.content) && message.content.length === 1
    && message.content[0]?.type === 'text' && typeof message.content[0].text === 'string', 'durable input must have one text block');
  return message.content[0].text;
}
function pollUrl(raw) {
  insist(typeof raw === 'string' && raw.startsWith('/api/v1/message?'), 'poll path or URL shape changed');
  const query = raw.slice('/api/v1/message?'.length);
  const fields = query.split('&');
  insist(fields.length === 3, 'poll query must have exactly three fields');
  const values = new Map();
  for (const field of fields) {
    const parts = field.split('=');
    insist(parts.length === 2 && ['password', 'after', 'limit'].includes(parts[0]) && !values.has(parts[0]), 'poll query has unknown, duplicate or malformed fields');
    values.set(parts[0], parts[1]);
  }
  insist(values.get('password') === 'core-channel-password' && values.get('limit') === '50', 'poll password or limit changed');
  const digits = values.get('after');
  insist(typeof digits === 'string' && /^[1-9]\d*$/.test(digits), 'after must be canonical positive decimal seconds');
  const epoch = Number(digits);
  insist(Number.isSafeInteger(epoch) && String(epoch) === digits && Number.isSafeInteger((epoch + 5) * 1000), 'after is not safe decimal seconds');
  return { epoch, replace(phase) {
    // Replace this field's decimal value only. Preserve raw order, spelling,
    // delimiters, path and every other byte; do not use URLSearchParams.
    return '/api/v1/message?' + fields.map((field) => field.startsWith('after=') ? `after=<core-channel-${phase}>` : field).join('&');
  } };
}

export function channelPollContract(raw, declaration) {
  insist(isDeepStrictEqual(declaration, DECLARATION), 'unsupported declaration or tolerance override');
  insist(raw?.scenario === SCENARIO && raw.evidence?.scenario?.name === SCENARIO
    && /^[a-f0-9]{64}$/.test(raw.evidence.scenario.sha256), 'scenario identity is missing or wrong');
  insist(Array.isArray(raw.steps) && raw.steps.length === 2, 'exactly two server phases are required');
  for (const [i, phase] of PHASES.entries()) {
    const step = raw.steps[i];
    insist(isDeepStrictEqual(step?.step?.serve, ['server', '--host', '127.0.0.1', '--port', '38411'])
      && step.step.timeout === 20000 && step.step.until === 'bluebubbles: polling '
      && step.step.then?.[0]?.sleep === phase.waitMs
      && step.then?.[0]?.step?.sleep === phase.waitMs && step.then[0].slept === true, 'server phase or its fixed startup/wait budget changed');
    insist(step.serve?.reached === true && step.serve.exitCode === 0 && step.serve.signal === null, 'server phase did not reach or exit successfully');
  }
  const files = Object.entries(raw.tree ?? {}).filter(([path, entry]) => path.endsWith(`/chats/${SESSION}.jsonl`) && Array.isArray(entry?.jsonl));
  insist(files.length === 1, 'exactly one actual durable channel transcript is required');
  const accepted = files[0][1].jsonl.filter((entry) => entry?.type === 'accepted_input' && entry.sessionId === SESSION);
  insist(accepted.length === 2, 'initial and recovered durable inputs are required exactly once');
  const times = accepted.map(stampEntry => stamp(stampEntry.createdAt));
  insist(times[1] > times[0], 'durable phase timestamps must increase');
  for (const [i, phase] of PHASES.entries()) insist(inputText(accepted[i]) === phase.text, 'durable initial/recovered phase text mismatch');
  const records = raw.provider?.['channel-poll'];
  insist(Array.isArray(records) && records.length === 7, 'poll request count must remain 4 + 3');
  const polls = [];
  const phases = [];
  let offset = 0;
  for (const [phaseIndex, phase] of PHASES.entries()) {
    let epoch;
    for (let i = 0; i < phase.count; i += 1) {
      const index = offset + i, record = records[index];
      insist(record?.method === 'GET' && record.headers && typeof record.headers === 'object' && !Array.isArray(record.headers)
        && isDeepStrictEqual(record.body, { raw: '' }), 'poll GET, headers or empty body shape changed');
      const url = pollUrl(record.url);
      epoch ??= url.epoch;
      insist(url.epoch === epoch, 'poll epoch changed inside a server phase');
      polls.push({ index, phase: phase.name, epoch, url: record.url, normalizedUrl: url.replace(phase.name), path: `$.provider.channel-poll[${index}].url` });
    }
    const ageMs = times[phaseIndex] - (epoch + 5) * 1000;
    // The recorded scene allows 20s startup plus its 8s/5s wait. floor(start)
    // contributes at most 999ms. This validity bound rejects stale/future or
    // misassigned epochs; it is not a new exact timing parity requirement.
    const maxAgeMs = 20000 + phase.waitMs + 999;
    insist(Number.isSafeInteger(ageMs) && ageMs >= 0 && ageMs <= maxAgeMs, 'after epoch is outside its durable phase startup budget');
    phases.push({ phase: phase.name, epoch, acceptedAt: accepted[phaseIndex].createdAt, ageMs, maxAgeMs });
    offset += phase.count;
  }
  insist(phases[1].epoch > phases[0].epoch, 'recovered epoch must advance beyond initial epoch');
  return { scenarioSha256: raw.evidence.scenario.sha256, polls, phases };
}

export function channelPollVolatilePaths(rawA, rawB, declaration) {
  const left = channelPollContract(rawA, declaration), right = channelPollContract(rawB, declaration);
  insist(left.scenarioSha256 === right.scenarioSha256, 'different scenario/assertion identities cannot share this policy');
  const paths = new Map();
  for (const [i, poll] of left.polls.entries()) {
    if (poll.url !== right.polls[i].url && poll.normalizedUrl === right.polls[i].normalizedUrl) {
      paths.set(poll.path, `coreChannelPollAfter: validated ${poll.phase} startup epoch against durable accepted_input; only after decimal value varies (budget ${left.phases[poll.phase === 'initial' ? 0 : 1].maxAgeMs}ms including floor)`);
    }
  }
  return paths;
}
