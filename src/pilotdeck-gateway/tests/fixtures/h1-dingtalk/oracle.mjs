// Runs fixed-baseline adapter code with an in-memory SDK and fetch peer.
// No baseline files are modified and no production service is contacted.
import { pathToFileURL } from 'node:url';
import assert from 'node:assert/strict';
const root = process.env.PILOTDECK_TS_BASELINE;
assert(root, 'PILOTDECK_TS_BASELINE is required');
const messages = [], replies = [], logs = [], clients = [];
const sdk = { TOPIC_ROBOT: '/v1.0/im/bot/messages/get', EventAck: { SUCCESS: 'SUCCESS' }, DWClient: class {
  constructor(options) { this.options = options; clients.push(this); }
  registerAllEventListener(listener) { this.listener = listener; }
  async connect() {} disconnect() { this.disconnected = true; }
}};
globalThis.require = (name) => { assert.equal(name, 'dingtalk-stream'); return sdk; };
globalThis.fetch = async (url, request) => { replies.push({url, body: JSON.parse(request.body)}); return {ok:true}; };
const {DingTalkChannel} = await import(pathToFileURL(`${root}/dist/src/adapters/channel/dingtalk/DingTalkChannel.js`));
const {DingTalkSessionMapper} = await import(pathToFileURL(`${root}/dist/src/adapters/channel/dingtalk/DingTalkSessionMapper.js`));
const channel = new DingTalkChannel({clientId:' fixture ',clientSecret:' secret ',mapper:new DingTalkSessionMapper(undefined,()=> 'fixed')});
const handle = await channel.start({gateway:{async *submitTurn(request) { messages.push(request); yield {type:'assistant_text_delta',text:process.env.PDRS_ORACLE_FAULT?'wrong':'你好'}; yield {type:'tool_call_finished',ok:false,toolName:'mock'}; }},logger:{info:s=>logs.push(['info',s]),warn:s=>logs.push(['warn',s]),error:s=>logs.push(['error',s])}});
const input = (id, data, topic=sdk.TOPIC_ROBOT) => ({headers:{topic,messageId:id},data:JSON.stringify(data)});
for (const frame of [
 input('1',{text:{content:' hello '},conversationId:'room',sessionWebhook:'https://api.dingtalk.com/robot/send?access_token=fixture'}),
 input('1',{text:'duplicate',conversationId:'room'}),
 input('2',{richText:[{text:'rich'},{text:'text'}],senderId:'user',sessionWebhook:'http://127.0.0.1/not-trusted'}),
 input('3',{text:'/new',conversationId:'room'}),
 input('4',{text:'follow',conversationId:'room'}),
 input('5',{text:'ignored',conversationId:'room'},'/ignored'),
]) { await channel.onDownstream(frame); }
await handle.stop('fixture');
assert.equal(messages.length,3);
assert.equal(messages[2].sessionKey,'dingtalk:chat=room:s_fixed');
assert.equal(replies.length,3);assert.equal(replies[0].body.markdown.text,'你好\n⚠️ mock 执行失败');
assert.equal(clients[0].disconnected,true);
process.stdout.write(JSON.stringify({options:clients[0].options,messages,replies,logs},null,2)+'\n');
