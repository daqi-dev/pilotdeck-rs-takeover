import assert from 'node:assert/strict';
import {pathToFileURL} from 'node:url';
const {DingTalkChannel}=await import(pathToFileURL(`${process.env.PILOTDECK_TS_BASELINE}/dist/src/adapters/channel/dingtalk/DingTalkChannel.js`));
const errors=[];
const handle=await new DingTalkChannel({clientId:'fixture',clientSecret:'fixture'}).start({gateway:{},logger:{error:s=>errors.push(s)}});
await handle.stop();
assert.deepEqual(errors,['dingtalk: dingtalk-stream not installed; run `npm install dingtalk-stream`']);
console.log(JSON.stringify(errors));
