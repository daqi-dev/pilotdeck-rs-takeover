import test from 'node:test';
import assert from 'node:assert/strict';
import { drive, verifyCase } from './capture.mjs';

const spec = { id: 'fixture', answers: [['问题一: ', 'yes'], ['问题二: ', 'done']], stdoutIncludes: 'saved', yamlIncludes: ['enabled: true'] };
const result = { exitCode: 0, signal: null, stdout: 'saved', stderr: '', answered: 2 };
const tree = { 'pilotdeck.yaml': 'enabled: true\n' };

test('required output, normal exit, prompt count and durable file mutations fail independently', () => {
  verifyCase(spec, result, tree);
  for (const mutation of [
    { exitCode: 2 }, { signal: 'SIGKILL' }, { stdout: 'fake' },
    { answered: 1 }, { problem: 'deadline' },
  ]) assert.throws(() => verifyCase(spec, { ...result, ...mutation }, tree));
  assert.throws(() => verifyCase(spec, result, {}));
  assert.throws(() => verifyCase(spec, result, { 'pilotdeck.yaml': 'enabled: false\n' }));
});

test('real child receives answers only after prompts, including split UTF8', async () => {
  const script = `
    const p = Buffer.from('问题一: ');
    process.stdout.write(p.subarray(0, 2));
    setTimeout(() => process.stdout.write(p.subarray(2)), 10);
    let n = 0;
    process.stdin.on('data', x => {
      if (++n === 1 && x.toString() === 'yes\\n') process.stdout.write('问题二: ');
      else if (n === 2 && x.toString() === 'done\\n') { process.stdout.write('saved'); process.exit(0); }
      else process.exit(7);
    });
  `;
  const captured = await drive(process.execPath, ['-e', script], {}, process.cwd(), spec.answers, 3000);
  verifyCase(spec, captured, tree);
});

test('real stalled child is terminated and cannot count as success', async () => {
  const captured = await drive(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], {}, process.cwd(), spec.answers, 50);
  assert.equal(captured.signal, 'SIGKILL');
  assert.match(captured.problem, /deadline/);
  assert.throws(() => verifyCase(spec, captured, tree));
});
