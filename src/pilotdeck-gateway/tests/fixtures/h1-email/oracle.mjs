// Execute the fixed baseline through package mocks; no sockets or account credentials.
import { writeFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
const baseline = process.argv[2];
const output = process.argv[3];
if (!baseline || !output) throw new Error('usage: oracle.mjs BASELINE RESULT');
const trace = [];
class Imap {
  constructor(config) { trace.push(['imap', config]); }
  async connect() { trace.push(['connect']); }
  async mailboxOpen(name) { trace.push(['open', name]); }
  async status() { return {uidNext: 4}; }
  async *fetch(query) {
    trace.push(['fetch', query]);
    if (typeof query === 'string') { yield {uid: 3}; return; }
    yield {uid: 3, envelope:{from:[{address:'a@example.test'}]},source:Buffer.from('old')};
    yield {uid: 4, envelope:{from:[{address:'a@example.test'}]},source:Buffer.from(' /new hello ')};
    yield {uid: 5, envelope:{from:[]},source:Buffer.from('ignored')};
  }
  async getMailboxLock(name) { trace.push(['lock', name]); return {release(){trace.push(['release']);}}; }
  async logout() { trace.push(['logout']); }
}
globalThis.require = (name) => {
  if (name === 'imapflow') return {ImapFlow:Imap};
  if (name === 'nodemailer') return { createTransport(config) {trace.push(['smtp',config]); return { async verify(){trace.push(['verify']);}, async sendMail(mail){trace.push(['mail',mail]);} }; } };
  throw new Error(`unexpected package ${name}`);
};
const url = (f) => pathToFileURL(`${baseline}/dist/src/adapters/channel/email/${f}.js`).href;
const {EmailChannel} = await import(url('EmailChannel'));
const {EmailSessionMapper} = await import(url('EmailSessionMapper'));
const {renderEmailEvent} = await import(url('email-render'));
const mapper = new EmailSessionMapper(undefined, () => 'fixed');
const mapped = [' hi ', '/new hi', 'next', '/new\tignored', '/new', 'after'].map(text => mapper.resolve({chatId:'a@example.test',text}));
const channel = new EmailChannel({extra:{address:'agent@example.test',password:'mock-password',imapHost:'imap.example.test',smtpHost:'smtp.example.test',imapTls:false,smtpTls:false},mapper:new EmailSessionMapper(undefined,()=> 'fixed')});
const bodies = ['  raw  ','content-type: text/plain\r\n\r\ncase', 'Content-Type: text/plain\r\n\r\nhello\r\n--abcdef1234', 'Content-Type: text/plain\r\nContent-Transfer-Encoding: quoted-printable\r\n=E4=BD=A0=\r\nline', 'Content-Type: text/html\r\n\r\n<b>html</b>', 'Content-Type: text/plain\r\n\r\nhello\r\n--ABCDEFGH'];
const extracted = bodies.map(raw => ({raw,text:channel.extractPlainText(raw)}));
const events = [{type:'assistant_text_delta',text:'hello'},{type:'assistant_thinking_delta',text:'secret'},{type:'tool_call_finished',ok:false,toolName:'bash'},{type:'error',message:'oops'},{type:'elicitation_request',questions:[{header:'Pick',question:'Which?',options:[{label:'One'},{label:'Two'}]}]},{type:'turn_finished'}];
const rendered = events.map(event => ({event,text:renderEmailEvent(event) ?? null}));
const handle = await channel.start({logger:{info(){},error(...args){trace.push(['error',...args]);}},gateway:{async *submitTurn(request){trace.push(['turn',request]);yield {type:'assistant_text_delta',text:'reply'};}}});
await new Promise(r=>setTimeout(r,10));
await handle.stop('test');
writeFileSync(output,JSON.stringify({mapped,extracted,rendered,trace},null,2)+'\n');
