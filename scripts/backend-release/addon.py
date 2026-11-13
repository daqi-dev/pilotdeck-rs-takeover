#!/usr/bin/env python3
"""Archive explicitly selected plugin resources; never duplicate core binaries or claim runtimes included."""
import argparse
import shutil
from common import Path, archive, digest, inventory, write_json
from resource_layout import resource_contract


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--plugins', type=Path, required=True)
    parser.add_argument('--plugin', action='append', required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--source-date-epoch', type=int, required=True)
    args = parser.parse_args()
    root = args.plugins.resolve(strict=True)
    inventory(root)
    if args.out.exists():
        parser.error('output directory must be new')
    if len(set(args.plugin)) != len(args.plugin):
        parser.error('duplicate plugin selection')
    for name in args.plugin:
        if Path(name).name != name or name in {'', '.', '..'} or not (root / name / 'plugin.json').is_file():
            parser.error('plugin selection must name an existing direct plugin directory')
    args.out.mkdir(parents=True)
    package = args.out / 'pilotdeck-explicit-plugin-addon'
    plugins = package / 'builtin-plugins'
    plugins.mkdir(parents=True)
    license_file = root / 'LICENSE'
    if not license_file.is_file():
        raise ValueError('resource addon requires its authored-source LICENSE')
    shutil.copy2(license_file, plugins / 'LICENSE')
    for name in args.plugin:
        selected = root / name
        for path in selected.rglob('*'):
            if path.name == 'node_modules':
                raise ValueError('resource addon cannot silently include installed runtimes')
            if path.is_file():
                with path.open('rb') as stream:
                    header = stream.read(4)
                if header in {b'\x7fELF', b'\xcf\xfa\xed\xfe', b'\xfe\xed\xfa\xcf', b'\xca\xfe\xba\xbe'} or header[:2] == b'MZ':
                    raise ValueError('resource addon cannot silently include native executables')
        shutil.copytree(selected, plugins / name, symlinks=True)
    contract = resource_contract(None, plugins, addon=True)
    write_json(package / 'manifest.json', {'schema': 1, 'kind': 'explicit-plugin-resource-addon',
        'classification': 'resource-candidate', 'resourceContract': contract,
        'installation': 'explicitly merge selected builtin-plugins into an installation; discovery enables them by default',
        'containsCoreBinaries': False, 'containsExternalRuntimes': False,
        'pendingDelivery': ['provision and identify external runtimes, browsers/models separately', 'verify final moved installation and each selected platform'],
        'files': inventory(package)})
    rows = inventory(package)
    (package / 'SHA256SUMS').write_text(''.join(f'{row["sha256"]}  {row["path"]}\n' for row in rows if row['type'] == 'file'))
    target = args.out / 'pilotdeck-explicit-plugin-addon.tar.gz'
    archive(package, target, args.source_date_epoch, package.name)
    write_json(args.out / 'archive.json', {'path': target.name, 'sha256': digest(target)})
    print(args.out / 'archive.json')


if __name__ == '__main__':
    main()
