#!/usr/bin/env python3
"""Normalize an existing SRT transcript; no audio capture or inferred minutes."""
import argparse
import json
from pathlib import Path
import re
import sys

STAMP = re.compile(r'^(\d{2,}):(\d{2}):(\d{2})[,.](\d{1,3})$')


def seconds(value):
    match = STAMP.fullmatch(value.strip())
    if not match:
        raise ValueError('invalid subtitle timestamp')
    h, m, s = map(int, match.groups()[:3])
    if m >= 60 or s >= 60:
        raise ValueError('invalid subtitle timestamp')
    return h * 3600 + m * 60 + s + int(match[4].ljust(3, '0')) / 1000


def parse(text):
    result = []
    for block in re.split(r'\n\s*\n', text.replace('\r\n', '\n').strip()):
        lines = block.splitlines()
        if not lines:
            continue
        index = 1 if lines[0].strip().isdigit() else 0
        if index >= len(lines) or '-->' not in lines[index]:
            raise ValueError('subtitle block lacks time range')
        left, right = lines[index].split('-->', 1)
        start, end = seconds(left), seconds(right)
        body = '\n'.join(lines[index + 1:]).strip()
        if end < start or not body:
            raise ValueError('invalid subtitle range or empty text')
        result.append({'start': start, 'end': end, 'text': body})
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('input', type=Path)
    args = parser.parse_args()
    try:
        rows = parse(args.input.read_text(encoding='utf-8-sig'))
        print(json.dumps({'text': '\n'.join(row['text'] for row in rows), 'segments': rows}, ensure_ascii=False, indent=2))
    except (OSError, ValueError) as error:
        print(f'transcript failed: {error}', file=sys.stderr)
        raise SystemExit(1)
