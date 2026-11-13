// Optional actual-capture test input. Never falls back to a developer's home.
import fs from'node:fs';import path from'node:path';import assert from'node:assert/strict';import{createHash}from'node:crypto';
const file=process.env.BBX_OBSERVER_ACTUAL_MANIFEST;
export const actualManifest=file?JSON.parse(fs.readFileSync(file,'utf8')):null;
if(actualManifest){assert.equal(actualManifest.schema,1);assert.ok(actualManifest.rawSha256&&actualManifest.observer);}
export function readActual(file){assert.ok(actualManifest,'BBX_OBSERVER_ACTUAL_MANIFEST required');const resolved=path.resolve(file);const expected=actualManifest.rawSha256[file]??actualManifest.rawSha256[resolved];assert.match(expected??'',/^[a-f0-9]{64}$/,'explicit original capture SHA');const bytes=fs.readFileSync(resolved);assert.equal(createHash('sha256').update(bytes).digest('hex'),expected,'actual capture unchanged');return JSON.parse(bytes);}
export const observerFiles=actualManifest?.observer;
export const baselineRoot=process.env.BBX_BASELINE;
