#!/usr/bin/env python3
"""Check two isolated non-root containers can bind the same CLI port."""
import argparse
import json
import selectors
import subprocess
import time
import uuid

parser = argparse.ArgumentParser()
parser.add_argument('--node', required=True, help='Absolute path of an installed Linux Node binary')
parser.add_argument('--image', default='debian:bookworm-slim')
args = parser.parse_args()
names = ['pdrs-bbx-isolation-' + uuid.uuid4().hex for _ in range(2)]
children = []
script = """const fs=require('node:fs'),net=require('node:net');
if(process.getuid()===0)throw Error('root');
if(fs.readdirSync('/sys/class/net').join(',')!=='lo')throw Error('network isolation');
fs.mkdirSync('/tmp/home/.pilotdeck',{recursive:true});
const server=net.createServer();
server.listen(18789,'127.0.0.1',()=>{console.log('BOUND');setTimeout(()=>server.close(),3000)});
"""
try:
    for name in names:
        children.append(subprocess.Popen([
            'docker', 'run', '--rm', '--name', name, '--network', 'none',
            '--user', '1000:1000', '--read-only', '--cap-drop', 'ALL',
            '--security-opt', 'no-new-privileges', '--tmpfs', '/tmp:rw,nosuid,nodev,size=32m',
            '--env', 'HOME=/tmp/home', '--env', 'PILOT_HOME=/tmp/home/.pilotdeck',
            '--mount', f'type=bind,src={args.node},dst=/usr/local/bin/node,readonly',
            args.image, 'node', '-e', script,
        ], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True))
    selector = selectors.DefaultSelector()
    for child in children:
        selector.register(child.stdout, selectors.EVENT_READ, child)
    deadline = time.monotonic() + 30
    while selector.get_map():
        if time.monotonic() >= deadline:
            raise RuntimeError('containers did not bind within 30 seconds')
        for key, _ in selector.select(timeout=1):
            line = key.fileobj.readline().strip()
            if line != 'BOUND':
                raise RuntimeError(f'container failed: {line}')
            selector.unregister(key.fileobj)
    for name in names:
        data = json.loads(subprocess.check_output(['docker', 'inspect', name]))[0]
        assert data['Config']['User'] == '1000:1000'
        assert data['HostConfig']['NetworkMode'] == 'none'
        assert not data['HostConfig']['Privileged']
        assert not data['HostConfig']['PortBindings']
    for child in children:
        stdout, stderr = child.communicate(timeout=15)
        if child.returncode:
            raise RuntimeError(f'container exit {child.returncode}: {stdout} {stderr}')
    print('PASS: two non-root private networks bound 18789 concurrently; no host port mapping')
finally:
    for name in names:
        subprocess.run(['docker', 'rm', '-f', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for child in children:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=10)
remaining = subprocess.check_output(['docker', 'ps', '-a', '--format', '{{.Names}}'], text=True).splitlines()
assert not set(names).intersection(remaining)
print('PASS: task containers removed')
