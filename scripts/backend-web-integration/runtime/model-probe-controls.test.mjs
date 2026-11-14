import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { identity, transformModelProbe } from './overlay.mjs';

const target = identity['ui/server/services/modelConnectionProbe.js'];
const original = readFileSync(target.path, 'utf8');
const patched = transformModelProbe(original);
const { probeModelConnection } = await import(pathToFileURL(target.path).href);
const sample = "messages.content.type 参数非法，取值范围 ['text']";
const calls = [];
let reply;
const realFetch = globalThis.fetch;
globalThis.fetch = async (url, options) => {
    calls.push({ url: String(url), options });
    assert.equal(typeof reply, 'function', 'every fetch must use the prepared local mock');
    return reply();
};
test.after(() => { globalThis.fetch = realFetch; });

async function probe(detail, { status = 400, image = true } = {}) {
    calls.length = 0;
    reply = () => new Response(JSON.stringify({ error: { message: detail } }), { status, headers: { 'content-type': 'application/json' } });
    const result = await probeModelConnection({
        protocol: 'openai', baseUrl: 'https://mock.invalid/v1', model: 'synthetic',
        apiKey: 'synthetic-not-a-credential', image, retryPolicy: { maxRetries: 0 },
    });
    assert.equal(calls.length, 1);
    assert.equal(calls[0].url, 'https://mock.invalid/v1/chat/completions');
    assert.equal(JSON.parse(calls[0].options.body).max_tokens, 4096, 'probe budget stays unchanged');
    return result;
}

test('real Chinese text-only 400 image diagnostic becomes unsupported', async () => {
    const result = await probe(sample);
    assert.equal(result.imageUnsupported, true);
    assert.equal(result.code, 'IMAGE_TEST_FAILED');
    assert.equal(result.error, sample);
    const body = JSON.parse(calls[0].options.body);
    assert.ok(body.messages[0].content.some(part => part.type === 'image_url'), 'existing image construction stays intact');
});

test('finite whitespace, comma and quoted-text formatting variants are recognized', async () => {
    for (const detail of [
        ' messages.content.type 参数非法, 取值范围: [ "text" ] ',
        'messages.content.type 参数非法，取值范围：[\'text\']',
    ]) assert.equal((await probe(detail)).imageUnsupported, true);
});

test('different field, image-allowing values, unknown schema and quota errors remain undetermined', async () => {
    for (const detail of [
        "messages.role 参数非法，取值范围 ['text']",
        "messages.content.type 参数非法，取值范围 ['text', 'image']",
        "messages.content.type 参数非法，取值范围 ['image', 'text']",
        "messages.content.type 参数非法，取值范围 ['unknown']",
        "messages.content.type 参数非法，取值范围 ['text'] rate limit exceeded",
        'messages.content.type 参数非法，额度不足',
        'rate limit exceeded', 'unknown bad request',
    ]) assert.equal((await probe(detail)).imageUnsupported, false, detail);
});

test('matching Chinese diagnostic on authentication, rate limit or server status is not capability evidence', async () => {
    for (const status of [401, 403, 404, 429, 500]) {
        const result = await probe(sample, { status });
        assert.equal(result.imageUnsupported, false, `HTTP ${status}`);
        if (status === 401 || status === 403) assert.equal(result.code, 'INVALID_API_KEY');
    }
});

test('text failure does not become image unsupported and English recognition stays intact', async () => {
    assert.equal((await probe(sample, { image: false })).imageUnsupported, false);
    assert.equal((await probe("content.type allowed values: ['text']" )).imageUnsupported, true);
});

test('actual sequential text success then image rejection keeps text passed and image unsupported', async () => {
    calls.length = 0;
    let step = 0;
    reply = () => ++step === 1
        ? new Response(JSON.stringify({ choices: [{ message: { content: '1' }, finish_reason: 'stop' }] }), { status: 200 })
        : new Response(JSON.stringify({ error: { message: sample } }), { status: 400 });
    const common = { protocol: 'openai', baseUrl: 'https://mock.invalid/v1', model: 'synthetic', apiKey: 'synthetic', retryPolicy: { maxRetries: 0 } };
    const text = await probeModelConnection(common);
    assert.equal(text.ok, true);
    const image = await probeModelConnection({ ...common, endpointUrl: text.endpointUrl, image: true });
    assert.equal(image.imageUnsupported, true);
    assert.equal(calls.length, 2);
});

test('source SHA drift fails closed and deleting the Chinese branch reproduces old misclassification', () => {
    assert.throws(() => transformModelProbe(`${original}\n`), /SHA mismatch/);
    const start = original.indexOf('function looksLikeImageUnsupported(detail) {');
    const end = original.indexOf('\nfunction classifyProbeError', start);
    const legacy = new Function(`${original.slice(start, end)}; return looksLikeImageUnsupported;`)();
    assert.equal(legacy(sample), false, 'the exact real diagnostic misses in the baseline');
    assert.ok(patched.includes('maxTokens = 4096'));
    assert.ok(patched.includes('if (image && looksLikeImageUnsupported(detail, response.status))'));
});
