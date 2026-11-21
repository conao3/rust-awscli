"""Golden results of install_skill._check_archive and _safe_extract on
crafted packages.

Run with `PYTHONPATH=$AWSR_REFERENCE_SRC python3 gen_archives.py >
archives.json` in this directory.
"""

import base64
import io
import json
import os
import struct
import tempfile
import zipfile
import zlib

from awscli.customizations.agentregistry import install_skill
from awscli.customizations.agentregistry.utils import AgentRegistryJWTError

SKILL = b'---\nname: pdf-tools\n---\n# PDF\n'


def build(entries, method=zipfile.ZIP_DEFLATED):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, 'w', method) as z:
        for entry in entries:
            name, data = entry[0], entry[1]
            info = zipfile.ZipInfo(name)
            info.compress_type = method
            if len(entry) > 2:
                info.external_attr = entry[2] << 16
            z.writestr(info, data)
    return buf.getvalue()


def patch(data, old, new):
    return data.replace(old, new)


CASES = {
    'root skill': build([('SKILL.md', SKILL), ('scripts/run.sh', b'echo\n', 0o100775), ('docs/a.txt', b'a')]),
    'stored': build([('SKILL.md', SKILL)], zipfile.ZIP_STORED),
    'wrapped': build([('w/', b''), ('w/skill.md', SKILL), ('w/x/y.txt', b'y', 0o104777)]),
    'wrapper without SKILL.md': build([('w/a.txt', b'a')]),
    'two top levels': build([('a/SKILL.md', SKILL), ('b.txt', b'b')]),
    'root and enclosed': build([('SKILL.md', SKILL), ('a/SKILL.md', SKILL)]),
    'empty': build([]),
    'traversal': build([('SKILL.md', SKILL), ('../evil', b'x')]),
    'absolute': build([('SKILL.md', SKILL), ('/etc/x', b'x')]),
    'backslash traversal': build([('SKILL.md', SKILL), ('a\\..\\..\\x', b'x')]),
    'symlink': build([('SKILL.md', SKILL), ('link', b'/etc/passwd', 0o120777)]),
    'unportable': build([('SKILL.md', SKILL), ('a:b', b'x')]),
    'reserved marker': build([('SKILL.md', SKILL), ('.Agent-Registry-Skill.json', b'{}')]),
    'reserved metadata': build([('SKILL.md', SKILL), ('sub/.aws-skill-metadata', b'{}')]),
    'duplicate': build([('SKILL.md', SKILL), ('a.txt', b'1'), ('a.txt', b'2')]),
    'collision': build([('SKILL.md', SKILL), ('A.txt', b'1'), ('a.txt', b'2')]),
    'collision sharp s': build([('SKILL.md', SKILL), ('straße', b'1'), ('STRASSE', b'2')]),
    'casing twice': build([('SKILL.md', SKILL), ('skill.md', SKILL)]),
    'odd casing': build([('Skill.md', SKILL)]),
    'skill md directory': build([('SKILL.md/', b''), ('SKILL.md/x', b'x')]),
    'no name': build([('SKILL.md', b'---\ndescription: d\n---\n')]),
    'legacy name': build([('SKILL.md', b'---\nname: PDF_Tool\n---\n')]),
    'bad name': build([('SKILL.md', b'---\nname: a/b\n---\n')]),
    'not a zip': b'not a zip at all',
    'truncated': build([('SKILL.md', SKILL * 50)])[:60],
}

bad_crc = bytearray(build([('SKILL.md', SKILL), ('a.txt', b'hello')], zipfile.ZIP_STORED))
bad_crc[bad_crc.index(b'hello')] = ord('j')
CASES['bad crc'] = bytes(bad_crc)
encrypted = bytearray(build([('SKILL.md', SKILL)], zipfile.ZIP_STORED))
for sig in (b'PK\x03\x04', b'PK\x01\x02'):
    i = encrypted.index(sig)
    off = i + (6 if sig == b'PK\x03\x04' else 8)
    encrypted[off] |= 1
CASES['encrypted'] = bytes(encrypted)


def listing(root):
    out = []
    for d, dirs, files in sorted(os.walk(root)):
        dirs.sort()
        for n in sorted(dirs):
            p = os.path.join(d, n)
            out.append([os.path.relpath(p, root) + '/', oct(os.stat(p).st_mode & 0o7777), None])
        for n in sorted(files):
            p = os.path.join(d, n)
            out.append([os.path.relpath(p, root), oct(os.stat(p).st_mode & 0o7777), open(p, 'rb').read().decode('utf-8', 'replace')])
    return out


results = []
for name, data in CASES.items():
    with tempfile.TemporaryDirectory() as tmp:
        archive = os.path.join(tmp, 'package.zip')
        with open(archive, 'wb') as f:
            f.write(data)
        result = {'name': name, 'zip': base64.b64encode(data).decode()}
        try:
            result['skill'] = install_skill._check_archive(archive, 'shown')
            dest = os.path.join(tmp, 'out')
            install_skill._safe_extract(archive, dest)
            result['files'] = listing(dest)
        except AgentRegistryJWTError as e:
            result['error'] = str(e)
        results.append(result)
print(json.dumps(results, indent=1))
