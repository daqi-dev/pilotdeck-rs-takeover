// Execute the fixed compiled adapter with an injected external SDK boundary.
// No baseline source is copied and no live account or HTTP service is used.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
const root = process.env.H1_BASELINE;
if (!root || !process.env.PILOT_HOME || !process.env.HOME) throw Error('isolated environment required');
const sent = [], turns = [], handlers = {}, logs = [];
let channel;
class MatrixClient {
  async getUserId() { return '@bot:local'; }
  on(name, handler) { handlers[name] = handler; }
  async start() {}
  stop() {}
  async joinRoom(id) { sent.push({join:id}); }
  async sendMessage(room, content) { sent.push({room,content}); }
}
const realRequire = createRequire(import.meta.url);
globalThis.require = (name) => name === 'matrix-bot-sdk' ? { MatrixClient, SimpleFsStorageProvider: class {} } : realRequire(name);
const { MatrixChannel } = await import(pathToFileURL(`${root}/dist/src/adapters/channel/matrix/MatrixChannel.js`));
channel = new MatrixChannel({ homeserver:'http://mock.invalid/', accessToken:'fixture', pilotHome:process.env.PILOT_HOME });
await channel.start({logger:{info:t=>logs.push(t),error:t=>logs.push(t)},gateway:{async *submitTurn(turn) { turns.push(turn); yield {type:'assistant_text_delta',text:' answer '}; }}});
for (const event of [{sender:'@bot:local',content:{body:'self'}},{sender:'@a:local',content:{msgtype:'m.notice',body:'notice'}},{sender:'@a:local',content:{body:'edit','m.relates_to':{rel_type:'m.replace'}}},{sender:'@a:local',content:{body:' hello '}},{sender:'@a:local',content:{body:'/new'}},{sender:'@a:local',content:{body:'next'}}]) await channel.handleRoomMessage('!room:local',event);
assert.equal(turns.length,2); assert.equal(turns[0].sessionKey,'matrix:chat=!room:local:general');
assert.match(turns[1].sessionKey,/^matrix:chat=!room:local:s_/); turns[1].sessionKey='matrix:chat=!room:local:s_<uuid>';
await channel.sendReply('!room:local','a'.repeat(3000)+' '+'b'.repeat(2000));
if (process.argv.includes('--negative')) sent.at(-2).content.body = 'corrupted';
assert.equal(sent.at(-2).content.body.length,3000); assert.equal(sent.at(-1).content.body[0],' ');
console.log(JSON.stringify({turns,sent:sent.map(v=>v.content?.body.length>100?{room:v.room,length:v.content.body.length,startsSpace:v.content.body[0]===' '}:v),logs},null,2));
