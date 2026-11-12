#!/usr/bin/env python3
"""Prepare auditable source overlays; never edit/build the supplied checkout."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument('--source-root', type=Path, required=True)
parser.add_argument('--out', type=Path, required=True)
args = parser.parse_args()
root = args.source_root.resolve()
out = args.out.resolve()
if out.exists():
    raise SystemExit('--out must not exist')
assembly_path = Path('src/pilotdeck-gateway/src/native_process/assembly.rs')
services_path = Path('src/pilotdeck-gateway/src/native_process/services.rs')
assembly = (root / assembly_path).read_text()
services = (root / services_path).read_text()
anchor = 'let lease = maintenance_project.acquire(root.as_deref())?;'
if assembly.count(anchor) != 1:
    raise SystemExit('expected exactly one actual maintenance resolver acquire boundary')
report = 'self.report_maintenance_schedule_failure(run, error);'
if services.count(report) != 1:
    raise SystemExit('expected exactly one isolated after-turn failure report; implement the approved fix first')
fault = '''static O1_MAINTENANCE_FAULT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                        if root.as_ref().is_some_and(|path| path.ends_with("maintenance-fault-project"))
                            && !O1_MAINTENANCE_FAULT.swap(true, std::sync::atomic::Ordering::SeqCst)
                        {
                            eprintln!("O1_RESOLVE_INJECT:{}", root.as_ref().unwrap().display());
                            return Err(crate::runtime::project::RuntimeError::new("INJECTED_FORMAL_RESOLVE_FAILURE")
                                .with_js_error(crate::runtime::project::RuntimeJsError {
                                    name: "TypeError".into(), code: None,
                                    message: "INJECTED_FORMAL_RESOLVE_FAILURE".into(), requires_host: false,
                                }));
                        }
                        '''
assembly = assembly.replace(anchor, fault + anchor)
sha = lambda value: hashlib.sha256(value.encode()).hexdigest()
source_sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
for variant in ('positive', 'negative'):
    destination = out / variant
    version = services if variant == 'positive' else services.replace(report, 'return Err(failure(error.to_string()));')
    files = {assembly_path: assembly, services_path: version}
    for relative, content in files.items():
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)
    manifest = {
        'boundary': 'maintenance-resolver-before-acquire',
        'projectBasename': 'maintenance-fault-project',
        'variant': variant,
        'sourceRoot': str(root),
        'sourceSha': source_sha,
        'originalFiles': {str(p): hashlib.sha256((root / p).read_bytes()).hexdigest() for p in files},
        'overlayFiles': {str(p): sha(content) for p, content in files.items()},
        'negativeMutation': None if variant == 'positive' else 'restore synchronous after-turn SubmitError propagation',
        'buildStatus': 'not built; copy overlays only into a dedicated source snapshot and record resulting binary SHA',
    }
    (destination / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
print(json.dumps({'prepared': str(out), 'built': False}))
