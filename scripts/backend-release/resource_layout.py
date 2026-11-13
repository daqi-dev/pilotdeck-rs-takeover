"""Describe resource inputs without changing loader defaults or installing dependencies."""
import json
from common import Path, inventory

EXTERNAL = {
    'pilotdeck-browser-runtime': ['Node', 'pinned Playwright MCP runtime', 'browser executable'],
    'funasr': ['Node', 'Python 3 on POSIX', 'source-built SenseVoice engine', 'licensed acoustic and VAD models'],
}


def describe_plugins(root):
    if root is None:
        return []
    root = Path(root).resolve(strict=True)
    inventory(root)
    plugins = []
    for directory in sorted(root.iterdir()):
        if not directory.is_dir():
            continue
        spec = directory / 'plugin.json'
        if not spec.is_file():
            raise ValueError(f'plugin directory lacks plugin.json: {directory.name}')
        value = json.loads(spec.read_text())
        name = value.get('name')
        allowed_identity = name == directory.name or (directory.name, name) == ('browser-use', 'pilotdeck-browser-runtime')
        if not isinstance(name, str) or not name.strip() or not allowed_identity or name in {p['name'] for p in plugins}:
            raise ValueError('plugin name must match its unique directory name or the exact browser runtime addon identity')
        servers = value.get('mcpServers', {})
        if not isinstance(servers, dict):
            raise ValueError('mcpServers must be an object')
        node = name in EXTERNAL or any('node' in str(s.get('command', '')).lower() for s in servers.values() if isinstance(s, dict))
        executable = bool(servers or value.get('hooks'))
        plugins.append({'name': name, 'version': value.get('version'), 'mcpServers': sorted(servers),
                        'defaultEnabledWhenDiscovered': True, 'requiresNodeOnDiscovery': node,
                        'executableIntegration': executable,
                        'externalDependencies': EXTERNAL.get(name, ['plugin-specific executable dependencies'] if executable else []),
                        'externalDependenciesIncluded': False})
    return plugins


def resource_contract(skills, plugins, *, addon=False):
    described = describe_plugins(plugins)
    if not addon and any(p['executableIntegration'] for p in described):
        raise ValueError('core/full resource input must contain only skill plugins; package executable plugins as an explicit addon')
    names = []
    if skills is not None:
        inventory(Path(skills))
        names = sorted(p.name for p in Path(skills).iterdir() if p.is_dir() and (p / 'SKILL.md').is_file())
    return {'schema': 1, 'skillNames': names, 'plugins': described,
            'requiresNodeOnDiscovery': any(p['requiresNodeOnDiscovery'] for p in described),
            'optionalAddonsIncluded': [p['name'] for p in described if p['executableIntegration']],
            'externalDependenciesIncluded': False,
            'defaultLoaderSemantics': 'discovered builtins enabled unless explicitly disabled',
            'verification': 'file inventory only; actual tool and moved-package verification required'}
