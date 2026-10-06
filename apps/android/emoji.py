#!/usr/bin/env python3
"""Regenerates app/src/main/assets/emoji.txt from Unicode's emoji-test.txt.

    apps/android/emoji.py path/to/emoji-test.txt

Fully-qualified emoji only, by group ("@Group" lines), one per line:
emoji, name and (for people) its skin-tone variants, tab-separated.
"""
import re
import sys
from pathlib import Path

TONE = r':? ?((light|medium-light|medium|medium-dark|dark) skin tone,? ?)+$'

groups, cur = [], None
for line in open(sys.argv[1], encoding='utf-8'):
    if line.startswith('# group:'):
        g = line.split(':', 1)[1].strip()
        cur = None if g == 'Component' else (g, [], {})
        if cur:
            groups.append(cur)
        continue
    if not cur or line.startswith('#') or not line.strip():
        continue
    m = re.match(r'^([0-9A-F ]+);\s*fully-qualified\s*#\s*(\S+)\s+E[\d.]+\s+(.*)$', line.strip())
    if not m:
        continue
    cps, name = m.group(1).split(), m.group(3)
    e = ''.join(chr(int(c, 16)) for c in cps)
    if any(0x1F3FB <= int(c, 16) <= 0x1F3FF for c in cps):
        i = cur[2].get(re.sub(TONE, '', name).rstrip(': ,'))
        if i is not None:
            cur[1][i][2].append(e)
        continue
    cur[2][name] = len(cur[1])
    cur[1].append([e, name, []])

out = []
for g, items, _ in groups:
    out.append('@' + g)
    out += [e + '\t' + n + ('\t' + ' '.join(v) if v else '') for e, n, v in items]
dest = Path(__file__).parent / 'app/src/main/assets/emoji.txt'
dest.write_text('\n'.join(out) + '\n', encoding='utf-8')
