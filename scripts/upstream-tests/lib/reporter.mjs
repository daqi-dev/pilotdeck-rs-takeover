// node --test 的自定义 reporter：每个顶层用例的结果写一行 JSON 到 PDRS_UT_REPORT，
// 同时把失败的错误信息原样打到 stdout 方便看。
import { appendFileSync } from 'node:fs';

export default async function* reporter(source) {
  const file = process.env.PDRS_UT_REPORT;
  for await (const event of source) {
    if (event.type !== 'test:pass' && event.type !== 'test:fail') continue;
    const data = event.data;
    if (data.nesting !== 0) continue;
    const row = {
      name: data.name,
      status: data.skip !== undefined ? 'skip' : event.type === 'test:pass' ? 'pass' : 'fail',
      skip: typeof data.skip === 'string' ? data.skip : undefined,
      durationMs: Math.round(data.details?.duration_ms ?? 0),
      file: data.file,
    };
    if (row.status === 'fail') {
      const error = data.details?.error;
      const cause = error?.cause ?? error;
      row.error = String(cause?.stack ?? cause?.message ?? cause ?? error).slice(0, 4000);
      yield `FAIL ${data.name}\n${row.error}\n\n`;
    }
    if (file) appendFileSync(file, JSON.stringify(row) + '\n');
  }
}
