// Fixture-only helpers. They do not import or execute either product implementation.
import fs from 'node:fs';
import { DatabaseSync } from 'node:sqlite';

export function matchesProviderRule(rule, { method, url, body }) {
  const pathname = new URL(url, 'http://bbx.invalid').pathname;
  return (rule.method === undefined || rule.method === method)
    && (rule.path === undefined || rule.path === pathname)
    && (rule.when ?? []).every((s) => body.includes(s))
    && (rule.unless ?? []).every((s) => !body.includes(s));
}

export function writeSeedFile(target, content, subst, substDeep) {
  if (content && typeof content === 'object' && !Array.isArray(content)
    && Object.keys(content).length === 1 && Object.hasOwn(content, '$binaryBase64')) {
    const value = content.$binaryBase64;
    if (typeof value !== 'string' || !/^[A-Za-z0-9+/]*={0,2}$/.test(value)) throw Error('$binaryBase64 seed must be canonical base64');
    const bytes = Buffer.from(value, 'base64');
    if (bytes.toString('base64') !== value) throw Error('$binaryBase64 seed must be canonical base64');
    fs.writeFileSync(target, bytes);
  } else if (content && typeof content === 'object' && !Array.isArray(content)
    && Object.keys(content).length === 1 && Object.hasOwn(content, '$sqlite')) {
    if (!Array.isArray(content.$sqlite) || content.$sqlite.some((sql) => typeof sql !== 'string')) throw Error('$sqlite seed must contain an array of SQL strings');
    const db = new DatabaseSync(target);
    try {
      db.exec('BEGIN');
      for (const sql of content.$sqlite) db.exec(subst(sql));
      db.exec('COMMIT');
    } finally { db.close(); }
  } else fs.writeFileSync(target, typeof content === 'string' ? subst(content) : JSON.stringify(substDeep(content), null, 2) + '\n');
}
