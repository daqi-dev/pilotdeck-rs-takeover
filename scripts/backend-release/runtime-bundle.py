#!/usr/bin/env python3
"""Record then assemble an explicitly identified existing extension runtime; never download or install tools."""
import argparse
import json
import os
import shutil
import struct
from common import Path, archive, digest, inventory, safe_name, write_json

BROWSER_FILES = {'node_modules', 'package.json', 'pnpm-lock.yaml', 'sources.json', 'runtime-manifest.json'}


def payload_inventory(root, kind):
    rows = inventory(root)
    if kind == 'browser':
        rows = [r for r in rows if Path(r['path']).parts[0] in BROWSER_FILES]
        required = ['node_modules/@playwright/mcp/cli.js', 'runtime-manifest.json', 'sources.json', 'pnpm-lock.yaml']
    else:
        required = ['models/sensevoice-small-q8.gguf', 'models/fsmn-vad.gguf', 'source-lock.json', 'licenses/Apache-2.0.txt']
    for name in required:
        if not (root / name).is_file():
            raise ValueError(f'missing runtime source/entry/license: {name}')
    if not rows:
        raise ValueError('empty runtime')
    return rows


def native_platform(file):
    with file.open('rb') as stream:
        header=stream.read(32)
        if header[:4] != b'\xcf\xfa\xed\xfe':
            raise ValueError('native supply currently supports identified thin Mach-O only')
        cpu=struct.unpack_from('<I',header,4)[0]
        commands=struct.unpack_from('<I',header,16)[0]
        for _ in range(commands):
            raw=stream.read(8);command,size=struct.unpack('<II',raw)
            if size < 8 or size > 1024*1024:raise ValueError('invalid native load command')
            body=stream.read(size-8)
            if command==0x32:
                platform,minimum=struct.unpack_from('<II',body)
                if platform!=1:raise ValueError('native engine is not macOS')
                return {'os':'darwin','arch':{0x100000c:'arm64',0x1000007:'x64'}.get(cpu,'unsupported'),
                        'minimumOS':f'{minimum>>16}.{(minimum>>8)&255}.{minimum&255}'}
    raise ValueError('native build-version load command missing')


def validate_sources(root, kind):
    if kind == 'browser':
        value = json.loads((root / 'runtime-manifest.json').read_text())
        if value.get('browserIncluded') or value.get('lifecycleScriptsExecuted') or value.get('network') != 'offline':
            raise ValueError('browser runtime must retain offline/no-browser/no-script provenance')
        if digest(root/'sources.json') != value['sourceManifestSha256'] or digest(root/'pnpm-lock.yaml') != value['lockSha256']:
            raise ValueError('browser source or dependency lock changed')
        for relative, expected in value['files'].items():
            safe_name(relative)
            file = root / relative
            actual = {'symlink':os.readlink(file)} if file.is_symlink() else digest(file)
            if actual != expected:
                raise ValueError('browser provision receipt differs from actual runtime')
        for package in value['packages']:
            target = (root / 'node_modules' / package['name']).resolve(strict=True)
            if not target.is_relative_to(root):
                raise ValueError('package escapes runtime')
            if digest(target / 'package.json') != package['packageSha256']:
                raise ValueError('package source identity changed')
            for name, expected in package['notices'].items():
                safe_name(name)
                if digest(target / name) != expected:
                    raise ValueError('package license/notice changed')
        return {'packages': [{k:v for k,v in p.items() if k != 'path'} for p in value['packages']], 'baseline':value['baseline']}
    lock = json.loads((root / 'source-lock.json').read_text())
    for model in lock['models']:
        safe_name(model['file'])
        file = root / 'models' / model['file']
        if file.stat().st_size != model['bytes'] or digest(file) != model['sha256']:
            raise ValueError('model differs from locked source identity')
    build = json.loads((root / 'build-result.json').read_text())
    candidates = list((root / 'runtime').rglob('llama-funasr-sensevoice'))
    if len(candidates) != 1 or digest(candidates[0]) != build['sha256']:
        raise ValueError('native engine differs from source-build receipt')
    licenses = list((root / 'licenses').rglob('*'))
    if not any(p.is_file() and p.name == 'LICENSE' for p in licenses):
        raise ValueError('native source licenses absent')
    return {'sources':lock['source'], 'models':lock['models'], 'nativeSha256':build['sha256'],'binaryPlatform':native_platform(candidates[0])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['record','build'])
    parser.add_argument('--kind', choices=['browser','funasr'], required=True)
    parser.add_argument('--input', type=Path, required=True)
    parser.add_argument('--identity', type=Path, required=True)
    parser.add_argument('--identity-sha256')
    parser.add_argument('--receipt', type=Path, action='append', default=[])
    parser.add_argument('--recipe', type=Path, action='append', default=[])
    parser.add_argument('--os')
    parser.add_argument('--arch')
    parser.add_argument('--minimum-os')
    parser.add_argument('--out', type=Path)
    parser.add_argument('--source-date-epoch', type=int, default=0)
    args = parser.parse_args()
    root = args.input.resolve(strict=True)
    rows = payload_inventory(root, args.kind)
    sources = validate_sources(root, args.kind)
    if args.action == 'record':
        if args.identity.exists() or not args.receipt or not args.recipe or not all([args.os,args.arch,args.minimum_os]):
            parser.error('record requires NEW identity, receipts, source recipes and explicit os/arch/minimum-os')
        if args.kind=='funasr':
            actual=sources['binaryPlatform']
            if args.os!=actual['os'] or args.arch!=actual['arch'] or tuple(map(int,args.minimum_os.split('.')))+(0,)*(3-len(args.minimum_os.split('.'))) != tuple(map(int,actual['minimumOS'].split('.'))):
                raise ValueError('declared platform/minimum OS differs from native Mach-O')
        extras=[]
        for role, paths in [('receipt',args.receipt),('source-recipe',args.recipe)]:
            for index,path in enumerate(paths):
                path=path.resolve(strict=True)
                if not path.is_file():
                    raise ValueError('receipt/recipe must be a regular file')
                extras.append({'role':role,'input':str(path),'path':f'provenance/{role}-{index}-{path.name}','sha256':digest(path)})
        value={'schema':1,'kind':args.kind,'platform':{'os':args.os,'arch':args.arch,'minimumOS':args.minimum_os},
               'sources':sources,'files':rows,'provenance':extras,
               'externalDependencies':['Node','Chrome or explicitly chosen supported browser'] if args.kind=='browser' else ['Node','Python 3','macOS system frameworks'],
               'installMapping':'set existing PILOTDECK_RUNTIME_ROOT to runtime/browser' if args.kind=='browser' else 'explicitly copy pilot-home/funasr into isolated PILOT_HOME/funasr',
               'containsNode':False,'containsBrowser':False,'containsPython':False}
        write_json(args.identity,value)
        print(digest(args.identity));return
    if not args.identity_sha256 or digest(args.identity) != args.identity_sha256:
        parser.error('build requires exact externally reviewed identity SHA256')
    value=json.loads(args.identity.read_text())
    if value['kind'] != args.kind or value['files'] != rows or value['sources'] != sources:
        raise ValueError('runtime changed after identity was recorded')
    if args.out is None or args.out.exists():
        parser.error('build requires NEW --out')
    ancestor=args.out.absolute().parent
    while not ancestor.exists(): ancestor=ancestor.parent
    if shutil.disk_usage(ancestor).free < 8*1024**3:
        raise ValueError('less than 8 GiB free before runtime copy/compression')
    if root == args.out.absolute() or args.out.absolute().is_relative_to(root):
        raise ValueError('output must be outside runtime input')
    args.out.mkdir(parents=True)
    package=args.out/f'pilotdeck-{args.kind}-runtime'
    payload=package/('runtime/browser' if args.kind=='browser' else 'pilot-home/funasr')
    payload.mkdir(parents=True)
    # Copy selected roots, preserving only previously validated relocatable symlinks.
    names=BROWSER_FILES if args.kind=='browser' else {p.name for p in root.iterdir()}
    for name in sorted(names):
        source=root/name
        if source.is_dir() and not source.is_symlink():shutil.copytree(source,payload/name,symlinks=True)
        elif source.is_symlink():os.symlink(os.readlink(source),payload/name)
        else:shutil.copy2(source,payload/name)
    if inventory(payload) != rows:
        raise ValueError('copied payload differs from reviewed identity')
    for item in value['provenance']:
        path=Path(item['input']);safe_name(item['path'])
        if digest(path) != item['sha256']:raise ValueError('source recipe or receipt changed')
        target=package/item['path'];target.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(path,target)
        if digest(target)!=item['sha256']:raise ValueError('copied provenance differs')
    # Source identity is retained, with no host path required for installation.
    copied={**value,'provenance':[{k:v for k,v in p.items() if k!='input'} for p in value['provenance']]}
    write_json(package/'runtime-identity.json',copied)
    write_json(package/'manifest.json',{'schema':1,'kind':'explicit-extension-runtime','runtimeKind':args.kind,
        'identityInputSha256':args.identity_sha256,'platform':value['platform'],
        'externalDependencies':value['externalDependencies'],'containsCoreBinaries':False,
        'containsNode':False,'containsBrowser':False,'containsPython':False,
        'installMapping':value['installMapping'],'files':inventory(package),
        'pendingDelivery':['final same-binary plugin/runtime moved-installation verification','platforms other than explicitly identified platform']})
    (package/'SHA256SUMS').write_text(''.join(f'{r["sha256"]}  {r["path"]}\n' for r in inventory(package) if r['type']=='file'))
    target=args.out/f'{package.name}.tar.gz'
    os.nice(10) # bounded low-priority compression, never competes as a build job
    archive(package,target,args.source_date_epoch,package.name)
    write_json(args.out/'archive.json',{'path':target.name,'sha256':digest(target)})
    print(args.out/'archive.json')


if __name__=='__main__':main()
