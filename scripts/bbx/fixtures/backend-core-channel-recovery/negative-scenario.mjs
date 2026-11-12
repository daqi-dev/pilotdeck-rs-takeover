#!/usr/bin/env node
// Real mock-model wrong-reply control; leaves product and harness untouched.
import fs from 'node:fs';import path from 'node:path';import {fileURLToPath} from 'node:url';
const source=path.resolve(path.dirname(fileURLToPath(import.meta.url)),'../../scenarios/backend-core-channel-recovery.json');
const out=process.argv[2];if(!out)throw Error('usage: negative-scenario.mjs new-file.json');
const scenario=JSON.parse(fs.readFileSync(source,'utf8'));scenario.provider.find(rule=>rule.name==='after-tool').reply.text='CORE-WRONG-REPLY';
fs.writeFileSync(out,JSON.stringify(scenario,null,2)+'\n',{flag:'wx'});
