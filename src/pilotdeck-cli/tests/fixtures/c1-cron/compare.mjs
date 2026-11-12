#!/usr/bin/env node
import { argumentsFor } from '../c1-entry/compare-common.mjs';
import { compareCaptured } from './compare-common.mjs';
try { process.exitCode = compareCaptured('cron', argumentsFor(process.argv.slice(2))); }
catch (error) { console.error(error.message); process.exitCode = 2; }
