// Runs the fixed TS methods; compact evidence preserves boundary code units.
const fs = require('node:fs');
const path = require('node:path');
async function run(name, body) {
  const source=fs.readFileSync(path.join(__dirname,'..',`h1-${name}`,'oracle.cjs'),'utf8').split('(async()=>{')[0];
  return await new Function('require', source+'\nreturn (async()=>{'+body+'})();')(require);
}
(async()=>{
  const telegram=await run('telegram', `const {TelegramChannel}=load(path.join(root,'src/adapters/channel/telegram/TelegramChannel.ts'));const channel=new TelegramChannel({token:'fixture'});channel.bot=new Bot('fixture');await channel.sendReply('42','x'.repeat(4095)+'\\n\\nrest');return calls.filter(c=>c[0]==='sendMessage').map(c=>({length:c[2].length,tail:c[2].slice(-5),lastUnit:c[2].charCodeAt(c[2].length-1)}));`);
  const qq=await run('qq', `const {QQChannel}=load(path.join(root,'src/adapters/channel/qq/QQChannel.ts'));const channel=new QQChannel({appId:'fixture',clientSecret:'fixture'});return channel.splitText('x'.repeat(1999)+'\\n\\nrest',2000).map(c=>({length:c.length,tail:c.slice(-5),lastUnit:c.charCodeAt(c.length-1)}));`);
  console.log(JSON.stringify({telegram,qq}));
})().catch(error=>{console.error(error);process.exitCode=1;});
