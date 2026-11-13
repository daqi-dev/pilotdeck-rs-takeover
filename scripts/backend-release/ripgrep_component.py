"""Validate and copy the mandatory source-built ripgrep component without executing it."""
import json
import shutil
import hashlib
import re
import tarfile
from common import Path, binary_target, digest, inventory, safe_name

RECORD = 'share/ripgrep-build.json'
SOURCE_PINS = {
    'ripgrep':'3a612f88b805e14aef45bfa43e25a54abc6297fc',
    'pcre2':'2dce7761b1831fd3f82a9c2bd5476259d945da4d',
    'sljit':'e51eabbfb8eabc6526f56e4e88b29fb10d1ee048',
    'Cargo.lock.sha256':'257bfa1eeacfa4d8222e2c50f9d2f8d15ef07e96adee46eaa87839f3b6cfa3b4',
}
LICENSE_PINS = {
 'ripgrep': {'0f96a83840e146e43c0ec96a22ec1f392e0680e6c1226e6f3ba87e0740af850f','7e12e5df4bae12cb21581ba157ced20e1986a0508dd10d0e8a4ab9a4cf94e85c'},
 'pcre2': {'9cf7ac6976099a1d856826d3ef1b093bd6b84489dc6100628ac79e740cf9885a'},
 'sljit': {'5f216505c0f6ea3273caec89e766eef93cdeb7bbb0c429f9360116d7c938feeb'},
}
REQUIRED_ROLES = {'rg-command','rg-exit','pcre-command','pcre-exit','platform','version','recipe'}


def checked_path(prefix, relative):
    safe_name(relative)
    if not relative or relative == '.' or not (prefix/relative).resolve(strict=True).is_relative_to(prefix):
        raise ValueError('ripgrep evidence/distribution path escapes prefix')
    return prefix/relative


def distribution_identity(prefix, relative):
    prefix = Path(prefix).resolve(strict=True)
    path = checked_path(prefix, relative)
    if path.is_symlink():
        raise ValueError('ripgrep distribution role cannot be a symlink')
    if path.is_file():
        return {'path':relative,'type':'file','sha256':digest(path)}
    if path.is_dir():
        rows = inventory(path)
        if not rows:
            raise ValueError('ripgrep distribution role cannot be empty')
        value = json.dumps(rows,sort_keys=True,separators=(',',':')).encode()
        return {'path':relative,'type':'directory','inventorySha256':hashlib.sha256(value).hexdigest()}
    raise ValueError('ripgrep distribution role must exist')


def validate_distribution(prefix, record):
    if record.get('sourcePins') != SOURCE_PINS:
        raise ValueError('ripgrep fixed source commits/Cargo.lock identity differs')
    distribution = record.get('distribution',{})
    entries=[]
    for role in ['ripgrep','pcre2','sljit']:
        source = distribution.get('sources',{}).get(role)
        notices = distribution.get('licenses',{}).get(role)
        if not isinstance(source,dict) or not isinstance(notices,list) or not notices:
            raise ValueError('ripgrep requires explicit rg/PCRE2/SLJIT source and license roles')
        if not any(entry.get('sha256') in LICENSE_PINS[role] for entry in notices):
            raise ValueError('ripgrep source-pinned license text is missing for '+role)
        entries += [source,*notices]
    lock=distribution.get('cargoLock');vendor=distribution.get('vendor')
    if not isinstance(lock,dict) or not isinstance(vendor,dict):
        raise ValueError('ripgrep requires explicit Cargo.lock and vendor distribution')
    entries += [lock,vendor]
    for entry in entries:
        if not isinstance(entry,dict) or entry != distribution_identity(prefix,entry.get('path','')):
            raise ValueError('ripgrep source/license/vendor role identity changed')
    if lock.get('type') != 'file' or lock.get('sha256') != SOURCE_PINS['Cargo.lock.sha256']:
        raise ValueError('ripgrep actual Cargo.lock differs from fixed source')
    vendor_path=prefix/vendor['path']
    if vendor['type']=='directory':
        complete=any(p.is_file() for p in vendor_path.rglob('.cargo-checksum.json'))
    else:
        try:
            with tarfile.open(vendor_path,'r:*') as source:
                complete=any(member.isfile() and member.name.endswith('/.cargo-checksum.json') for member in source)
        except (tarfile.TarError,OSError):
            complete=False
    if not complete:
        raise ValueError('ripgrep vendor distribution lacks Cargo checksum manifests')


def successful_exit(text):
    if text.strip()=='0': return True
    try: value=json.loads(text)
    except (ValueError,TypeError): return False
    return isinstance(value,dict) and type(value.get('exit')) is int and value['exit']==0


def validate_evidence(prefix, evidence):
    if not isinstance(evidence,list):
        raise ValueError('ripgrep requires retained role-bound build evidence')
    roles=[entry.get('role') for entry in evidence];paths=[entry.get('path') for entry in evidence]
    if len(roles)!=len(set(roles)) or len(paths)!=len(set(paths)) or not REQUIRED_ROLES.issubset(roles):
        raise ValueError('ripgrep requires distinct paths and unique required evidence roles')
    texts={}
    for entry in evidence:
        path=checked_path(prefix,entry.get('path',''))
        if not path.is_file() or path.is_symlink() or digest(path)!=entry.get('sha256'):
            raise ValueError('ripgrep retained build evidence changed or escaped')
        if entry['role'] in REQUIRED_ROLES:
            texts[entry['role']]=path.read_text()
    if not all(successful_exit(texts[role]) for role in ['rg-exit','pcre-exit']):
        raise ValueError('ripgrep build exit must be actual zero for rg and PCRE2')
    rg=texts['rg-command'];pcre=texts['pcre-command']
    compact=re.sub(r'[\s\"\']','',rg+'\n'+texts['recipe'])
    if not ('--locked' in rg and '--features' in rg and 'pcre2' in rg
            and re.search(r'PCRE2_SYS_STATIC[:=]0',compact)
            and re.search(r'PKG_CONFIG_ALL_STATIC[:=]1',compact)
            and 'PCRE2_SUPPORT_JIT=ON' in pcre and 'BUILD_SHARED_LIBS=OFF' in pcre):
        raise ValueError('ripgrep command/recipe lacks locked external-static PCRE2 JIT build flags')
    version=texts['version']
    if not re.search(r'^ripgrep 15\.0\.0(?:\s|$)',version) or 'PCRE2 10.45 is available (JIT is available)' not in version:
        raise ValueError('ripgrep actual version/PCRE2 JIT evidence differs')
    if not texts['platform'].strip() or not texts['recipe'].strip():
        raise ValueError('ripgrep platform and recipe evidence cannot be empty')


def validate_ripgrep(prefix, target):
    prefix = Path(prefix).resolve(strict=True)
    rows = inventory(prefix)
    record_file = prefix / RECORD
    if not record_file.is_file() or record_file.is_symlink():
        raise ValueError('ripgrep requires its source-build record')
    record = json.loads(record_file.read_text())
    executable = 'bin/rg.exe' if 'windows' in target else 'bin/rg'
    expected = {'schema':1, 'name':'ripgrep', 'version':'15.0.0', 'target':target, 'sourceBuilt':True}
    if any(record.get(key) != value for key,value in expected.items()) or record.get('pcre2') != {'version':'10.45','jit':True}:
        raise ValueError('ripgrep source/version/PCRE2 JIT/target identity differs from required contract')
    binary = prefix / executable
    if binary.is_symlink() or not binary.is_file() or record.get('binary') != {'path':executable,'sha256':digest(binary)}:
        raise ValueError('ripgrep binary differs from source-build record')
    if binary_target(binary) != target:
        raise ValueError('ripgrep architecture differs from release target')
    if not binary.stat().st_mode & 0o111:
        raise ValueError('ripgrep binary is not executable')
    observed = [row for row in rows if row['path'] != RECORD]
    if record.get('files') != observed:
        raise ValueError('ripgrep exact source/license/payload inventory differs from build record')
    for section in ['source','licenses']:
        directory = prefix / 'share' / section
        if not directory.is_dir() or not any(p.is_file() for p in directory.rglob('*')):
            raise ValueError(f'ripgrep is missing its {section} distribution')
    validate_evidence(prefix,record.get('buildEvidence'))
    validate_distribution(prefix,record)
    return {'name':'ripgrep','version':'15.0.0','binary':f'components/ripgrep/{executable}',
            'sha256':digest(binary),'pcre2':record['pcre2'],'sourceIncluded':True,'licensesIncluded':True,
            'buildRecord':f'components/ripgrep/{RECORD}','buildRecordSha256':digest(record_file)}


def copy_ripgrep(prefix, output, target):
    checked = validate_ripgrep(prefix,target)
    destination = output / 'components/ripgrep'
    shutil.copytree(prefix,destination,symlinks=True)
    if validate_ripgrep(destination,target) != checked:
        raise ValueError('copied ripgrep component differs from source-built input')
    return checked
