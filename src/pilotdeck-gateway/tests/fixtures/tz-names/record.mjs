// Records how the fixed TS baseline resolves time-zone names, for
// `tests/tz_names.rs`. Run from the repository root with Node v22.22.0:
//   node src/pilotdeck-gateway/tests/fixtures/tz-names/record.mjs /abs/PilotDeck > oracle.json
// The baseline must be the built checkout of cc7c4e6 (it imports `dist/`).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const baseline = process.argv[2];
assert.ok(baseline, "pass the built baseline checkout as the first argument");
assert.equal(process.versions.node, "22.22.0");
const here = dirname(fileURLToPath(import.meta.url));
const load = (name) => import(pathToFileURL(join(baseline, "dist/src", name)).href);
const { createGetCurrentTimeTool } = await load("tool/builtin/getCurrentTime.js");
const { isValidCronTimezone } = await load("cron/CronTimezone.js");
const { computeNextCronRunAt } = await load("cron/runtime/CronSchedule.js");

// 1. The baseline entry points for a hand-picked list of names.
const NOW = [1767225600000, 1782864000000]; // 2026-01-01T00:00Z, 2026-07-01T00:00Z
const CRON = "0 9 * * *";
const tool = createGetCurrentTimeTool();
const cases = [];
for (const name of JSON.parse(readFileSync(join(here, "names.json"), "utf8"))) {
  const results = [];
  for (const now of NOW) {
    try {
      results.push({ data: (await tool.execute({ timezone: name }, { now: () => new Date(now) })).data });
    } catch (error) {
      results.push({ error: { code: error.code, message: error.message } });
    }
  }
  const next = computeNextCronRunAt(CRON, new Date(NOW[1]), name);
  cases.push({ name, tool: results, cronValid: isValidCronTimezone(name), cronNext: next ? next.getTime() : null });
}

// 2. Acceptance of every candidate name (the same `Intl.DateTimeFormat`
//    constructor `isValidCronTimezone` and the tool use) and, for accepted
//    names, ICU's resolved zone.
const accepted = {};
const rejected = [];
for (const name of JSON.parse(readFileSync(join(here, "candidates.json"), "utf8"))) {
  try {
    accepted[name] = new Intl.DateTimeFormat("en-US", { timeZone: name }).resolvedOptions().timeZone;
  } catch {
    rejected.push(name);
  }
}

// 3. UTC offset (seconds) of every resolved zone at fixed samples:
//    the 15th of Jan/Apr/Jul/Oct at 12:00Z in the listed years.
const YEARS = [1900, 1925, 1945, 1960, 1970, 1980, 1990, 2000, 2010, 2020, 2024, 2025, 2026, 2027, 2030, 2037, 2050, 2099];
const samples = YEARS.flatMap((year) => [0, 3, 6, 9].map((month) => Date.UTC(year, month, 15, 12)));
const offsets = {};
for (const zone of [...new Set(Object.values(accepted))].sort()) {
  const format = new Intl.DateTimeFormat("en-US", {
    timeZone: zone, hourCycle: "h23", year: "numeric", month: "numeric", day: "numeric",
    hour: "numeric", minute: "numeric", second: "numeric",
  });
  offsets[zone] = samples.map((ms) => {
    const p = Object.fromEntries(format.formatToParts(new Date(ms)).map((x) => [x.type, x.value]));
    return (Date.UTC(+p.year, +p.month - 1, +p.day, +p.hour, +p.minute, +p.second) - ms) / 1000;
  });
}

const versions = { node: process.versions.node, icu: process.versions.icu, tz: process.versions.tz };
const oracle = { baseline: "cc7c4e6", versions, now: NOW, cron: CRON, cases, accepted, rejected, samples, offsets };
process.stdout.write(`${JSON.stringify(oracle)}\n`);
