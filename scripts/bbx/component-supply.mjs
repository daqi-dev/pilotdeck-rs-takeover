// Explicit native-component environment binding and read-only identity only.
// This module never resolves default engines, runs tools, or changes verdicts.
import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';

const bindings = [
  ['PILOTDECK_RIPGREP_PATH', null],
  ['PILOTDECK_PDF_TOOL', 'BBX_PDF_TOOL'],
  ['PILOTDECK_IMAGE_TOOL', 'BBX_IMAGE_TOOL'],
];

export function componentSupplyEnvironment(parent) {
  const supplied = {};
  for (const [variable, alias] of bindings) {
    const value = parent[variable] !== undefined ? parent[variable] : alias ? parent[alias] : undefined;
    // Preserve an explicitly empty override: do not silently restore an alias.
    if (value !== undefined) {
      if (typeof value !== 'string') throw new TypeError(`${variable} must be a string`);
      supplied[variable] = value;
    }
  }
  return supplied;
}

export function componentSupplyIdentity(environment) {
  const result = {};
  for (const [variable] of bindings) {
    if (environment[variable] === undefined) continue;
    const value = environment[variable];
    const identity = { value, realpath: null, sha256: null, error: null };
    result[variable] = identity;
    if (typeof value !== 'string') { identity.error = 'non_string_path'; continue; }
    if (value === '') { identity.error = 'explicitly_unset'; continue; }
    // A relative override is interpreted in the product child's cwd. Do not
    // pretend the harness's own cwd is the actual resolver result.
    if (!path.isAbsolute(value)) { identity.error = 'relative_path_not_resolved'; continue; }
    try {
      identity.realpath = fs.realpathSync(value);
      if (!fs.statSync(identity.realpath).isFile()) { identity.error = 'not_a_file'; continue; }
      identity.sha256 = createHash('sha256').update(fs.readFileSync(identity.realpath)).digest('hex');
    } catch (error) {
      identity.error = error.code || error.name;
    }
  }
  return result;
}
