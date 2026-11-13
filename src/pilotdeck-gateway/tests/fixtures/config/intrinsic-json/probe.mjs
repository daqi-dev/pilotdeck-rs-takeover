import { writeFileSync } from 'node:fs';
import { loadPilotConfig } from '../../../../../../resources/PilotDeck/dist/src/pilot/config/loadPilotConfig.js';
const [kind,key]=process.argv.slice(2);const home=process.env.PILOT_HOME;
if(!home||!kind||!key)throw Error('isolated PILOT_HOME, case and key required');
const provider=kind==='provider'?key:'parity',model=kind==='model'?key:'deterministic';
const cfg={schemaVersion:1,agent:{model:`${provider}/${model}`},model:{providers:{[provider]:{protocol:'openai',url:'http://127.0.0.1:28794/v1',apiKey:'mock-only',models:{[model]:{}}}}}};
if(kind==='always')cfg.alwaysOn=JSON.parse(`{${JSON.stringify(key)}:7}`);
if(kind==='project')cfg.alwaysOn={projects:{'/fixture':JSON.parse(`{${JSON.stringify(key)}:7}`)}};
const configPath=home+'/pilotdeck.yaml';writeFileSync(configPath,JSON.stringify(cfg));
try{const v=loadPilotConfig({configPath,env:{HOME:home,PILOT_HOME:home}});console.log(JSON.stringify({ok:true,diagnostics:v.diagnostics,model:v.config.model,agent:v.config.agent,alwaysOn:v.config.alwaysOn}));}
catch(e){console.log(JSON.stringify({ok:false,name:e.name,message:e.message,code:e.code}));}
