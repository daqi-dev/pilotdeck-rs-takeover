// Compare the source-built component with fixed baseline MuPDF on generated,
// credential-free PDFs. The output directory must be fresh.
import fs from 'node:fs';
import path from 'node:path';
import {pathToFileURL} from 'node:url';
import {spawnSync} from 'node:child_process';
import crypto from 'node:crypto';
const [helper, baseline, target, buildIdentity] = process.argv.slice(2);
if (!helper || !baseline || !target || !buildIdentity) throw new Error('usage: check-pdf.mjs HELPER BASELINE NEW_OUTPUT BUILD_IDENTITY_FILE');
const out = path.resolve(target);
fs.mkdirSync(out);
fs.mkdirSync(path.join(out, 'home', '.pilotdeck'), {recursive: true});
const env = {...process.env, HOME:path.join(out, 'home'), PILOT_HOME:path.join(out, 'home', '.pilotdeck')};
const baselineGit = spawnSync('git', ['-C', path.resolve(baseline), 'rev-parse', 'HEAD'], {env, encoding:'utf8'});
if (baselineGit.status !== 0 || baselineGit.stdout.trim() !== 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85') throw new Error('fixed TypeScript baseline is required');
const provenance = fs.readFileSync(buildIdentity, 'utf8');
const [mupdfSourceCommit, helperSourceCommit] = provenance.split('\n');
if (mupdfSourceCommit !== 'd3b7556577b790e9761868c314ad6fd9b6dd86a9' || !/^[a-f0-9]{40}$/.test(helperSourceCommit)) throw new Error('pinned component build identity is required');
const mupdfPackage = JSON.parse(fs.readFileSync(path.join(baseline, 'node_modules/mupdf/package.json'), 'utf8'));
if (mupdfPackage.version !== '1.27.0') throw new Error('MuPDF 1.27.0 baseline is required');
fs.writeFileSync(path.join(out, 'component-build-identity.txt'), provenance);
const mupdf = await import(pathToFileURL(path.join(baseline, 'node_modules/mupdf/dist/mupdf.js')).href);
function pdf(width, height) {
  const stream = 'BT /F1 18 Tf 40 70 Td (PilotDeck native PDF) Tj ET\n';
  const objects = ['<< /Type /Catalog /Pages 2 0 R >>', '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    `<< /Type /Page /Parent 2 0 R /MediaBox [0 0 ${width} ${height}] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>`,
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>', `<< /Length ${Buffer.byteLength(stream)} >>\nstream\n${stream}endstream`];
  let text = '%PDF-1.4\n'; const offsets = [0];
  for (const [i, object] of objects.entries()) { offsets.push(Buffer.byteLength(text)); text += `${i+1} 0 obj\n${object}\nendobj\n`; }
  const start = Buffer.byteLength(text);
  text += `xref\n0 ${objects.length+1}\n0000000000 65535 f \n`;
  text += offsets.slice(1).map(offset => `${String(offset).padStart(10, '0')} 00000 n \n`).join('');
  return Buffer.from(text + `trailer\n<< /Size ${objects.length+1} /Root 1 0 R >>\nstartxref\n${start}\n%%EOF\n`);
}
const source = pdf(240,120); const file = path.join(out, 'normal.pdf'); fs.writeFileSync(file, source);
const checks = [];
function run(name, ...args) {
  const result = spawnSync(path.resolve(helper), args, {env, timeout:30_000, maxBuffer:32*1024*1024});
  fs.writeFileSync(path.join(out, `${name}.stdout`), result.stdout ?? Buffer.alloc(0));
  fs.writeFileSync(path.join(out, `${name}.stderr`), result.stderr ?? Buffer.alloc(0));
  return result;
}
const doc = mupdf.Document.openDocument(source,'application/pdf');
try {
  const count = run('count', 'count', file);
  checks.push({name:'count',passed:count.status===0 && count.stdout.toString().trim()===String(doc.countPages())});
  const page = doc.loadPage(0);
  try {
    const text = page.toStructuredText();
    try { const actual=run('text','text',file); checks.push({name:'text',passed:actual.status===0 && actual.stdout.toString()===text.asText()}); }
    finally { text.destroy(); }
    const pixmap = page.toPixmap(mupdf.Matrix.scale(100/72,100/72),mupdf.ColorSpace.DeviceRGB,false,true);
    try {
      const expected=Buffer.from(pixmap.asJPEG(80,false)); fs.writeFileSync(path.join(out,'expected.jpg'),expected);
      const actual=run('render','render',file,'1');
      checks.push({name:'jpeg-byte-identity',passed:actual.status===0 && actual.stdout.equals(expected)});
    } finally { pixmap.destroy(); }
  } finally { page.destroy(); }
} finally { doc.destroy(); }
const invalid=run('page-out-of-range','render',file,'2'); checks.push({name:'out-of-range',passed:invalid.status===1 && !invalid.error && !invalid.signal && invalid.stdout.length===0});
const large=path.join(out,'large.pdf'); fs.writeFileSync(large,pdf(100000,100000));
const bounded=run('large-page','render',large,'1'); checks.push({name:'large-page-limit',passed:bounded.status===1 && !bounded.error && !bounded.signal && bounded.stdout.length===0 && bounded.stderr.toString().includes('pixel limit')});
const corrupt=path.join(out,'corrupt.pdf'); fs.writeFileSync(corrupt,'not a pdf');
const rejected=run('corrupt-pdf','count',corrupt); checks.push({name:'corrupt-pdf',passed:rejected.status===1 && !rejected.error && !rejected.signal && rejected.stdout.length===0});
const sha = value => crypto.createHash('sha256').update(value).digest('hex');
const manifest={node:process.version,helper:path.resolve(helper),helperSha256:sha(fs.readFileSync(helper)),helperSourceCommit,mupdfSourceCommit,componentBuildIdentitySha256:sha(provenance),baseline:path.resolve(baseline),baselineCommit:baselineGit.stdout.trim(),mupdfVersion:mupdfPackage.version,mupdfWasmSha256:sha(fs.readFileSync(path.join(baseline,'node_modules/mupdf/dist/mupdf-wasm.wasm'))),fixtures:[file,large,corrupt].map(file=>({name:path.basename(file),sha256:sha(fs.readFileSync(file))})),checks,passed:checks.every(c=>c.passed)};
fs.writeFileSync(path.join(out,'manifest.json'), JSON.stringify(manifest,null,2));
console.log(JSON.stringify(manifest));
if (!manifest.passed) process.exitCode=1;
