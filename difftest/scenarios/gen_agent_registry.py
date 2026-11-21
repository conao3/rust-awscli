"""Generate agent-registry.json: `python3 gen_agent_registry.py agent-registry.json`."""
import hashlib
import json
import sys

OUT = sys.argv[1]
REG = 'abcdefghijkl12'
ARN = f'arn:aws:agent-registry:us-east-1:123456789012:registry/{REG}'
EP = '{endpoint}'
JSON_HDR = {'Content-Type': 'application/json'}
UUID = '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}'


def summary(rid, name, rtype='SKILL', desc=None, version='1'):
    r = {
        'registryArn': ARN,
        'recordArn': f'{ARN}/record/{rid}',
        'recordId': rid,
        'name': name,
        'recordType': rtype,
        'recordVersion': version,
        'status': 'APPROVED',
        'createdAt': 1700000000,
        'updatedAt': 1700000000,
    }
    if desc is not None:
        r['description'] = desc
    return r


def full(rid, name, skill_md=None, artifact=None, link=None):
    r = summary(rid, name)
    md = {}
    if skill_md is not None:
        md['data'] = skill_md
    if link is not None:
        md['source'] = {'fromUrl': {'url': link}}
    r['descriptors'] = {
        'agentSkillsDefinition': {'additionalData': {'skillMd': md}}
    }
    if artifact is not None:
        r['artifact'] = artifact
    return r


def resp(body, status=200):
    return {'status': status, 'headers': JSON_HDR, 'body': json.dumps(body)}


def page(records, token=None):
    body = {'registryRecords': records}
    if token:
        body['nextToken'] = token
    return resp(body)


def batch(records, errors=()):
    return resp({'registryRecords': records, 'errors': list(errors)})


def md(name, body='# Skill\n'):
    return f'---\nname: {name}\ndescription: d\n---\n{body}'


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


def marker(rid, digest, content='skillMd', endpoint=EP, registry=REG):
    return json.dumps({
        'registryId': registry,
        'recordId': rid,
        'endpoint': endpoint,
        'sha256': digest,
        'content': content,
    }) + '\n'


def ar(*args):
    return ['agent-registry', *args]


S = []


def sc(name, argv, **kw):
    S.append({'name': f'agent-registry {name}', 'argv': argv, **kw})


EPA = ['--endpoint-url', EP]
CLAUDE = {'home/.claude/settings.json': '{}'}
SKILL = md('pdf-tools')

# --- arguments and validation -----------------------------------------------
sc('list-skills missing registry', ar('list-skills'))
sc('list-skills bad registry id', ar('list-skills', '--registry-id', 'bad!'))
sc('list-skills arn with bad id', ar('list-skills', '--registry-id', f'arn:aws:agent-registry:us-east-1:123456789012:registry/x'))
sc('list-skills bad agent', ar('list-skills', '--registry-id', REG, '--agent', 'nope'))
sc('list-skills bad authorizer', ar('list-skills', '--registry-id', REG, '--authorizer', 'jwt'))
sc('list-skills no region', ar('list-skills', '--registry-id', REG, *EPA),
   env={'AWS_CONFIG_FILE': '{casedir}/none'})
sc('list-skills jwt no verify ssl', ar('list-skills', '--registry-id', REG, '--authorizer', 'JWT', '--no-verify-ssl', *EPA))
sc('list-skills jwt over http', ar('list-skills', '--registry-id', REG, '--authorizer', 'JWT', *EPA))
sc('list-skills endpoint with query', ar('list-skills', '--registry-id', REG, '--endpoint-url', EP + '/base?x=1'))
sc('list-skills jwt endpoint with fragment', ar('list-skills', '--registry-id', REG, '--authorizer', 'JWT', '--endpoint-url', 'https://127.0.0.1:9#f'))
sc('list-skills jwt metadata unreachable', ar('list-skills', '--registry-id', REG, '--authorizer', 'JWT', '--endpoint-url', 'https://127.0.0.1:9/'))
sc('list-skills jwt metadata unreachable by arn', ar('list-skills', '--registry-id', f'{ARN}', '--authorizer', 'JWT', '--endpoint-url', 'https://127.0.0.1:9/base'))
sc('list-skills jwt bad callback port ignored for cache', ar('list-skills', '--registry-id', REG, '--authorizer', 'JWT', '--endpoint-url', 'https://127.0.0.1:9'),
   env={'AWS_AGENT_REGISTRY_CALLBACK_PORT': '+80'})
sc('list-skills jwt preset token', ar('list-skills', '--registry-id', REG, '--authorizer', 'JWT', '--endpoint-url', 'https://127.0.0.1:9', '--client-id', 'c'),
   env={'AWS_BEARER_TOKEN_AGENT_REGISTRY': 'preset-token-0123456789'})
sc('list-skills jwt preset token over http', ar('list-skills', '--registry-id', REG, '--authorizer', 'JWT', *EPA),
   env={'AWS_BEARER_TOKEN_AGENT_REGISTRY': 'preset-token-0123456789'})
sc('install-skill neither record arg', ar('install-skill', '--registry-id', REG))
sc('install-skill both record args', ar('install-skill', '--registry-id', REG, '--record-id', 'rec000000001', '--record-name', 'x'))
sc('install-skill agent and target', ar('install-skill', '--registry-id', REG, '--record-id', 'rec000000001', '--agent', 'kiro', '--target-directory', 'd'))
sc('install-skill no agents', ar('install-skill', '--registry-id', REG, '--record-id', 'rec000000001'))
sc('install-skill undetected agent', ar('install-skill', '--registry-id', REG, '--record-id', 'rec000000001', '--agent', 'kiro'))
sc('install-skill missing registry', ar('install-skill', '--record-id', 'rec000000001'))
sc('uninstall-skill missing name', ar('uninstall-skill'))
sc('uninstall-skill bad name', ar('uninstall-skill', '--name', '../x'))
sc('uninstall-skill long name', ar('uninstall-skill', '--name', 'a' * 65))
sc('uninstall-skill no agents', ar('uninstall-skill', '--name', 'pdf-tools'))
sc('uninstall-skill not installed', ar('uninstall-skill', '--name', 'pdf-tools'), files=CLAUDE)
sc('uninstall-skill agent and target', ar('uninstall-skill', '--name', 'x', '--agent', 'cursor', '--target-directory', 'd'))
sc('sign-out neither', ar('sign-out'))
sc('sign-out both', ar('sign-out', '--all', '--registry-id', REG))
sc('sign-out bad registry', ar('sign-out', '--registry-id', 'nope'))
sc('sign-out all empty', ar('sign-out', '--all'))
sc('sign-out none cached', ar('sign-out', '--registry-id', REG))
sc('sign-out none cached at endpoint', ar('sign-out', '--registry-id', REG, '--endpoint-url', 'https://Registry.Example:443/'))
sc('sign-out none cached at endpoint without region', ar('sign-out', '--registry-id', REG, '--endpoint-url', 'https://registry.example/'),
   env={'AWS_CONFIG_FILE': '{casedir}/none'})
sc('sign-out http endpoint', ar('sign-out', '--registry-id', REG, *EPA))
sc('sign-out http endpoint without region', ar('sign-out', '--registry-id', REG, *EPA),
   env={'AWS_CONFIG_FILE': '{casedir}/none'})

# --- list-skills with SigV4 -------------------------------------------------
P1 = [summary('rec000000001', 'pdf tools', desc='Extract\ttext\x1b[31m'),
      summary('rec000000002', 'mcp', rtype='MCP')]
P2 = [summary('rec000000003', 'notes', desc='Notes'),
      summary('rec000000004', 'no-md')]
B = [full('rec000000001', 'pdf tools', SKILL),
     full('rec000000003', 'notes', md('Notes_Legacy')),
     full('rec000000004', 'no-md', link='https://example.com/x.md')]
INSTALLED = {
    'home/.claude/skills/whatever/SKILL.md': SKILL,
    'home/.claude/skills/whatever/.agent-registry-skill.json': marker('rec000000001', 'old'),
    'home/.agents/skills/notes/SKILL.md': 'x',
    'home/.agents/skills/notes/.agent-registry-skill.json': marker('rec000000003', 'x', endpoint=EP + '/'),
    'home/.gemini/settings.json': '{}',
    'home/.cursor/skills/pdf/SKILL.md': 'x',
    'home/.cursor/skills/pdf/.agent-registry-skill.json': marker('rec000000001', 'x', registry='otherregistry1'),
    'home/.kiro/skills/.hidden/SKILL.md': 'x',
    'home/.kiro/skills/.hidden/.agent-registry-skill.json': marker('rec000000001', 'x'),
}
LIST = ar('list-skills', '--registry-id', REG, *EPA)
sc('list-skills', LIST, files=INSTALLED, responses=[page(P1, 't1'), page(P2), batch(B)])
sc('list-skills user agent', LIST, files=INSTALLED, responses=[page(P1), batch(B[:1])], user_agent_command=True, user_agent_features=True)
sc('list-skills text', [*LIST, '--output', 'text'], files=INSTALLED, responses=[page(P1, 't1'), page(P2), batch(B)])
sc('list-skills table', [*LIST, '--output', 'table'], files=INSTALLED, responses=[page(P1), batch(B[:1])])
sc('list-skills query', [*LIST, '--query', 'skills[].skillName'], files=INSTALLED, responses=[page(P1), batch(B[:1])])
sc('list-skills agent filter', [*LIST, '--agent', 'Claude-Code'], files=INSTALLED, responses=[page(P1), batch(B[:1])])
sc('list-skills no agents', LIST, responses=[page(P1), batch(B[:1])])
sc('list-skills by arn', ar('list-skills', '--registry-id', ARN, *EPA), files=INSTALLED, responses=[page(P1), batch(B[:1])])
sc('list-skills empty', LIST, responses=[page([])])
sc('list-skills names fail', LIST, files=INSTALLED,
   responses=[page(P1), resp({'message': 'denied'}, 403) | {'headers': {**JSON_HDR, 'x-amzn-ErrorType': 'AccessDeniedException'}}])
sc('list-skills listing fails', LIST, responses=[resp({'message': 'nope'}, 404) | {'headers': {**JSON_HDR, 'x-amzn-ErrorType': 'ResourceNotFoundException'}}])
sc('list-skills sigv4 authorizer', [*LIST, '--authorizer', 'SIGV4'], responses=[page(P1), batch(B[:1])])
sc('list-skills record without id', LIST,
   responses=[page([{k: v for k, v in summary('x', 'noid').items()}]), batch([])])

# --- install-skill with SigV4 from SKILL.md text ------------------------------
TD = ['--target-directory', 'skills']
INST = ar('install-skill', '--registry-id', REG, '--record-id', 'rec000000001', *EPA)
OUTS = ['skills/pdf-tools/SKILL.md', 'skills/pdf-tools/.agent-registry-skill.json']
sc('install-skill skill md', [*INST, *TD], responses=[batch(B[:1])], outputs=OUTS)
sc('install-skill user agent', [*INST, *TD], responses=[batch(B[:1])], user_agent_command=True, user_agent_features=True)
sc('install-skill skill md by arn', ar('install-skill', '--registry-id', REG, '--record-id', f'{ARN}/record/rec000000001', *EPA, *TD),
   responses=[batch(B[:1])], outputs=OUTS)
sc('install-skill by name', ar('install-skill', '--registry-id', REG, '--record-name', 'pdf tools', *EPA, *TD),
   responses=[page(P1, 't1'), page(P2), batch(B[:1])], outputs=OUTS)
sc('install-skill by name missing', ar('install-skill', '--registry-id', REG, '--record-name', 'pdf-tools', *EPA, *TD),
   responses=[page(P1)])
sc('install-skill by name twice', ar('install-skill', '--registry-id', REG, '--record-name', 'pdf tools', *EPA, *TD),
   responses=[page(P1 + [summary('rec000000009', 'pdf tools', version='')])])
sc('install-skill detected agents', INST, files={**CLAUDE, 'home/.cursor/x': '', 'home/.codex/x': '', 'home/.gemini/x': ''},
   responses=[batch(B[:1])],
   outputs=['home/.claude/skills/pdf-tools/SKILL.md', 'home/.cursor/skills/pdf-tools/.agent-registry-skill.json', 'home/.agents/skills/pdf-tools/SKILL.md'])
sc('install-skill agent', [*INST, '--agent', 'cursor'], files={**CLAUDE, 'home/.cursor/x': ''},
   responses=[batch(B[:1])], outputs=['home/.claude/skills/pdf-tools/SKILL.md', 'home/.cursor/skills/pdf-tools/SKILL.md'])
DIGEST = sha(SKILL)
sc('install-skill up to date', [*INST, *TD],
   files={'skills/old-name/SKILL.md': SKILL, 'skills/old-name/.agent-registry-skill.json': marker('rec000000001', DIGEST)},
   responses=[batch(B[:1])], outputs=OUTS + ['skills/old-name/SKILL.md'])
sc('install-skill update', [*INST, *TD],
   files={'skills/pdf-tools/SKILL.md': 'old', 'skills/pdf-tools/.agent-registry-skill.json': marker('rec000000001', 'f' * 64),
          'skills/older/SKILL.md': 'old', 'skills/older/.agent-registry-skill.json': marker('rec000000001', 'e' * 64)},
   responses=[batch(B[:1])], outputs=OUTS + ['skills/older/SKILL.md'])
sc('install-skill update from package kept', [*INST, *TD],
   files={'skills/pdf-tools/SKILL.md': 'old', 'skills/pdf-tools/.agent-registry-skill.json': marker('rec000000001', 'f' * 64, content='package')},
   responses=[batch(B[:1])], outputs=OUTS)
sc('install-skill force replaces package', [*INST, *TD, '--force'],
   files={'skills/pdf-tools/SKILL.md': 'old', 'skills/pdf-tools/.agent-registry-skill.json': json.dumps({'registryId': REG, 'recordId': 'rec000000001', 'endpoint': EP, 'sha256': 'f' * 64})},
   responses=[batch(B[:1])], outputs=OUTS)
sc('install-skill force reinstall', [*INST, *TD, '--force'],
   files={'skills/pdf-tools/SKILL.md': SKILL, 'skills/pdf-tools/.agent-registry-skill.json': marker('rec000000001', DIGEST)},
   responses=[batch(B[:1])], outputs=OUTS)
sc('install-skill newer content', [*INST, *TD],
   files={'skills/x/SKILL.md': SKILL, 'skills/x/.agent-registry-skill.json': marker('rec000000001', DIGEST, content='zip2')},
   responses=[batch(B[:1])], outputs=['skills/x/SKILL.md'])
sc('install-skill not a skill folder', [*INST, *TD], files={'skills/pdf-tools/README': 'x'},
   responses=[batch(B[:1])], outputs=['skills/pdf-tools/README', 'skills/pdf-tools/SKILL.md'])
sc('install-skill no marker', [*INST, *TD], files={'skills/pdf-tools/SKILL.md': 'mine'},
   responses=[batch(B[:1])], outputs=['skills/pdf-tools/SKILL.md'])
sc('install-skill other registry', [*INST, *TD],
   files={'skills/pdf-tools/SKILL.md': 'x', 'skills/pdf-tools/.agent-registry-skill.json': marker('rec000000001', 'x', registry='otherregistry1')},
   responses=[batch(B[:1])], outputs=['skills/pdf-tools/SKILL.md'])
sc('install-skill other endpoint', [*INST, *TD],
   files={'skills/pdf-tools/SKILL.md': 'x', 'skills/pdf-tools/.agent-registry-skill.json': marker('rec000000001', 'x', endpoint='https://other.example')},
   responses=[batch(B[:1])], outputs=['skills/pdf-tools/SKILL.md'])
sc('install-skill other endpoint agent', [*INST],
   files={**CLAUDE, 'home/.claude/skills/pdf-tools/SKILL.md': 'x', 'home/.claude/skills/pdf-tools/.agent-registry-skill.json': marker('rec000000002', 'x')},
   responses=[batch(B[:1])], outputs=['home/.claude/skills/pdf-tools/SKILL.md'])
sc('install-skill same record name', [*INST, *TD], responses=[batch([full('rec000000001', 'pdf-tools', SKILL)])], outputs=OUTS)
sc('install-skill legacy name', [*INST, *TD], responses=[batch([full('rec000000001', 'x', md('PDF_Tool'))])], outputs=['skills/PDF_Tool/SKILL.md'])
for label, text in [
    ('invalid name', md('a/b')),
    ('no frontmatter', '# just text'),
    ('blank lines first', '\n\n---\nname: x\n---\n'),
    ('crlf only', '---\rname: x\r---\r'),
    ('no name', '---\ndescription: d\n---\n'),
    ('bad yaml', '---\nname: [x\n---\n'),
    ('duplicate name', '---\nname: a\nname: b\n---\n'),
    ('unterminated', '---\nname: x\n'),
    ('nul', '---\nname: x\n---\n\x00'),
]:
    sc(f'install-skill skill md {label}', [*INST, *TD], responses=[batch([full('rec000000001', 'x', text)])])
sc('install-skill skill md too long', [*INST, *TD], responses=[batch([full('rec000000001', 'x', md('x') + 'a' * 102400)])])
sc('install-skill link only', [*INST, *TD], responses=[batch([full('rec000000001', 'x', link='https://u:p@Example.com:8443/s.md?sig=1')])])
sc('install-skill nothing', [*INST, *TD], responses=[batch([full('rec000000001', 'x', '   ')])])
sc('install-skill bad digest', [*INST, *TD], responses=[batch([full('rec000000001', 'x', SKILL, artifact={'downloadUrl': 'https://b.s3.us-east-1.amazonaws.com/k', 'sha256': 'ABC'})])])
sc('install-skill download not s3', [*INST, *TD], responses=[batch([full('rec000000001', 'x', SKILL, artifact={'downloadUrl': 'https://evil.example/k?X-Amz-Signature=s', 'sha256': 'a' * 64})])])
sc('install-skill download over http', [*INST, *TD], responses=[batch([full('rec000000001', 'x', SKILL, artifact={'downloadUrl': 'http://b.s3.us-east-1.amazonaws.com/k?X-Amz-Signature=s', 'sha256': 'a' * 64})])])
sc('install-skill download in another region', [*INST, *TD], responses=[batch([full('rec000000001', 'x', SKILL, artifact={'downloadUrl': 'https://b.s3.us-west-2.amazonaws.com/k', 'sha256': 'a' * 64})])])
sc('install-skill package up to date', [*INST, *TD],
   files={'skills/pdf-tools/SKILL.md': 'x', 'skills/pdf-tools/.agent-registry-skill.json': marker('rec000000001', 'a' * 64, content='package')},
   responses=[batch([full('rec000000001', 'x', SKILL, artifact={'downloadUrl': 'https://b.s3.us-east-1.amazonaws.com/k', 'sha256': 'a' * 64})])])
sc('install-skill batch errors', [*INST, *TD], responses=[batch([], [{'registryId': REG, 'recordId': 'rec000000001', 'errorCode': 'NotFound', 'message': 'gone\x1b[2J'}])])
sc('install-skill other record', [*INST, *TD], responses=[batch([full('rec000000009', 'x', SKILL)])])
sc('install-skill not found', [*INST, *TD], responses=[batch([])])
sc('install-skill batch fails', [*INST, *TD], responses=[resp({'message': 'no'}, 403) | {'headers': {**JSON_HDR, 'x-amzn-ErrorType': 'AccessDeniedException'}}])
sc('install-skill unreadable registry after fetch', ar('install-skill', '--registry-id', 'arn:aws:agent-registry:us-east-1:123456789012:registry/abc', '--record-id', 'rec000000001', *EPA, *TD),
   responses=[batch(B[:1])])

# --- uninstall-skill ---------------------------------------------------------
M1 = marker('rec000000001', DIGEST)
sc('uninstall-skill target', ar('uninstall-skill', '--name', 'pdf-tools', '--target-directory', 'skills', '--yes'),
   files={'skills/pdf-tools/SKILL.md': 'x', 'skills/pdf-tools/.agent-registry-skill.json': M1}, outputs=['skills/pdf-tools/SKILL.md'])
sc('uninstall-skill not a terminal', ar('uninstall-skill', '--name', 'pdf-tools', '--target-directory', 'skills'),
   files={'skills/pdf-tools/SKILL.md': 'x', 'skills/pdf-tools/.agent-registry-skill.json': M1}, outputs=['skills/pdf-tools/SKILL.md'])
sc('uninstall-skill agents', ar('uninstall-skill', '--name', 'pdf-tools', '--yes'),
   files={**CLAUDE, 'home/.claude/skills/pdf-tools/SKILL.md': 'x', 'home/.claude/skills/pdf-tools/.agent-registry-skill.json': M1,
          'home/.cursor/skills/pdf-tools/SKILL.md': 'x',
          'home/.kiro/skills/pdf-tools/README': 'x',
          'home/.agents/skills/pdf-tools/SKILL.md': 'x', 'home/.agents/skills/pdf-tools/.agent-registry-skill.json': marker('r', 'x', registry='other')},
   outputs=['home/.claude/skills/pdf-tools/SKILL.md', 'home/.cursor/skills/pdf-tools/SKILL.md', 'home/.agents/skills/pdf-tools/SKILL.md'])
sc('uninstall-skill agent filter', ar('uninstall-skill', '--name', 'pdf-tools', '--agent', 'codex', '--yes'),
   files={'home/.codex/x': '', 'home/.agents/skills/pdf-tools/SKILL.md': 'x', 'home/.agents/skills/pdf-tools/.agent-registry-skill.json': M1},
   outputs=['home/.agents/skills/pdf-tools/SKILL.md'])
sc('uninstall-skill not installed for agent', ar('uninstall-skill', '--name', 'pdf-tools', '--agent', 'codex'), files={'home/.codex/x': ''})
sc('uninstall-skill only skipped', ar('uninstall-skill', '--name', 'pdf-tools', '--yes'),
   files={**CLAUDE, 'home/.claude/skills/pdf-tools/SKILL.md': 'x'}, outputs=['home/.claude/skills/pdf-tools/SKILL.md'])
sc('uninstall-skill a file', ar('uninstall-skill', '--name', 'pdf-tools', '--target-directory', 'skills', '--yes'),
   files={'skills/pdf-tools': 'x'})
sc('uninstall-skill legacy name', ar('uninstall-skill', '--name', 'PDF_Tool', '--target-directory', 'skills', '--yes'),
   files={'skills/PDF_Tool/SKILL.md': 'x', 'skills/PDF_Tool/.agent-registry-skill.json': marker('rec000000001', 'x', content='package')}, outputs=['skills/PDF_Tool/SKILL.md'])
sc('uninstall-skill bad marker', ar('uninstall-skill', '--name', 'pdf-tools', '--target-directory', 'skills', '--yes'),
   files={'skills/pdf-tools/SKILL.md': 'x', 'skills/pdf-tools/.agent-registry-skill.json': '{"registryId": 1}'}, outputs=['skills/pdf-tools/SKILL.md'])

# --- sign-out ----------------------------------------------------------------
CACHE = 'home/.aws/agent-registry/cache'
PENDING = f'{CACHE}/pending-revocation'
REAL_EP = 'https://agent-registry.us-east-1.api.aws'


def entry_name(endpoint, registry):
    return hashlib.sha256(f'{endpoint}|{registry}'.encode()).hexdigest() + '.json'


def entry(endpoint=REAL_EP, registry=REG, **extra):
    return json.dumps({'registryId': registry, 'endpoint': endpoint, 'clientId': 'client', 'issuer': 'https://idp.example',
                       'accessToken': 'eyJa.eyJb.c', 'expiresAt': 1, **extra}, indent=2)


E1 = f'{CACHE}/{entry_name(REAL_EP, REG)}'
E2 = f'{CACHE}/{entry_name("https://other.example", REG)}'
E3 = f'{CACHE}/{entry_name(REAL_EP, "otherregistry1")}'
P1F = f'{PENDING}/{REG}.{entry_name(REAL_EP, REG)}'
DEAD = 'https://127.0.0.1:9/revoke'
sc('sign-out no refresh token', ar('sign-out', '--registry-id', REG), files={E1: entry()}, outputs=[E1])
sc('sign-out no revocation endpoint', ar('sign-out', '--registry-id', ARN), files={E1: entry(refreshToken='r' * 20)}, outputs=[E1])
sc('sign-out revocation fails', ar('sign-out', '--registry-id', REG),
   files={E1: entry(refreshToken='refresh-token-1', revocationEndpoint=DEAD), E3: entry(registry='otherregistry1')},
   outputs=[E1, P1F, E3])
sc('sign-out revocation over http', ar('sign-out', '--registry-id', REG),
   files={E1: entry(refreshToken='refresh-token-1', revocationEndpoint='http://idp.example/revoke')}, outputs=[E1, P1F])
sc('sign-out revocation fails then retried', ar('sign-out', '--registry-id', REG),
   setup=[ar('sign-out', '--registry-id', REG)],
   files={E1: entry(refreshToken='refresh-token-1', revocationEndpoint=DEAD)}, outputs=[E1, P1F])
sc('sign-out pending kept twice', ar('sign-out', '--registry-id', REG),
   files={E1: entry(refreshToken='refresh-token-2', revocationEndpoint=DEAD),
          P1F: json.dumps({'registryId': REG, 'endpoint': REAL_EP, 'tokens': [{'refreshToken': 'refresh-token-1', 'revocationEndpoint': DEAD, 'clientId': 'client'}]}, indent=2)},
   outputs=[E1, P1F])
sc('sign-out endpoint filter', ar('sign-out', '--registry-id', REG, '--endpoint-url', 'https://Other.Example:443/'),
   files={E1: entry(), E2: entry(endpoint='https://other.example')}, outputs=[E1, E2])
sc('sign-out endpoint filter without region', ar('sign-out', '--registry-id', REG, '--endpoint-url', 'https://other.example'),
   env={'AWS_CONFIG_FILE': '{casedir}/none'},
   files={E1: entry(), E2: entry(endpoint='https://other.example')}, outputs=[E1, E2])
sc('sign-out unreadable entry', ar('sign-out', '--all'), files={E1: '{not json', f'{CACHE}/notes.txt': 'x'}, outputs=[E1, f'{CACHE}/notes.txt'])
sc('sign-out all', ar('sign-out', '--all'),
   files={E1: entry(), E2: entry(endpoint='https://other.example', refreshToken='rt-other-123', revocationEndpoint=DEAD), E3: entry(registry='otherregistry1'),
          f'{CACHE}/.tmp-abc.json': '{}', f'{PENDING}/.tmp-def.json': '{}'},
   outputs=[E1, E2, E3, f'{CACHE}/.tmp-abc.json', f'{PENDING}/.tmp-def.json'])
sc('sign-out unreadable pending', ar('sign-out', '--registry-id', REG),
   files={P1F: '[]', f'{PENDING}/otherregistry1.{entry_name(REAL_EP, "otherregistry1")}': '{"registryId": "otherregistry1"}',
          f'{PENDING}/{entry_name(REAL_EP, "x")}': 'nope'},
   outputs=[P1F])
sc('sign-out misnamed pending', ar('sign-out', '--all'),
   files={P1F: json.dumps({'registryId': REG, 'endpoint': 'https://moved.example', 'tokens': [{'refreshToken': 'r', 'revocationEndpoint': DEAD}]})},
   outputs=[P1F])
sc('sign-out pending without tokens', ar('sign-out', '--registry-id', REG),
   files={P1F: json.dumps({'registryId': REG, 'endpoint': REAL_EP, 'tokens': []})}, outputs=[P1F])

# --- bearer selection and the handlers on generated commands ------------------
LDR = ar('list-discoverable-registry-records', '--registry-id', REG, '--region', 'us-east-1')
sc('generated bearer over http', [*LDR, *EPA],
   env={'AWS_BEARER_TOKEN_AGENT_REGISTRY': 'preset-token-0123456789', 'AWS_AUTH_SCHEME_PREFERENCE': 'httpBearerAuth'})
sc('generated bearer without token', [*LDR, *EPA], env={'AWS_AUTH_SCHEME_PREFERENCE': 'httpBearerAuth'})
sc('generated bearer no verify ssl', [*LDR, '--endpoint-url', 'https://127.0.0.1:9', '--no-verify-ssl'],
   env={'AWS_BEARER_TOKEN_AGENT_REGISTRY': 'preset-token-0123456789', 'AWS_AUTH_SCHEME_PREFERENCE': 'httpBearerAuth'})
sc('generated sigv4 no verify ssl', [*LDR, *EPA, '--no-verify-ssl'], responses=[page([])])
sc('generated sigv4 preference', [*LDR, *EPA], env={'AWS_AUTH_SCHEME_PREFERENCE': 'sigv4,httpBearerAuth'}, responses=[page([])])
sc('list-skills bearer preference no verify ssl', [*LIST, '--no-verify-ssl'],
   env={'AWS_BEARER_TOKEN_AGENT_REGISTRY': 'preset-token-0123456789', 'AWS_AUTH_SCHEME_PREFERENCE': 'httpBearerAuth'})
PRESIGNED = 'https://b.s3.us-east-1.amazonaws.com/k?X-Amz-Credential=AKID%2Fx&X-Amz-Security-Token=tok&X-Amz-Signature=deadbeef'
BG = ar('batch-get-discoverable-registry-record', '--entries', f'registryId={REG},recordIds=rec000000001', '--region', 'us-east-1', *EPA)
BGBODY = batch([full('rec000000001', 'pdf tools', SKILL, artifact={'downloadUrl': PRESIGNED, 'sha256': 'a' * 64})])
DEBUG_MASKS = ["(?i)'date': '[^']*'", "'Server': '[^']*'", "exe/[^ \\n]*", "Signature=[0-9a-f]{64}", "X-Amz-Date': b?'[0-9TZ]*'", "20[0-9]{6}T[0-9]{6}Z", "[0-9]{8}/us-east-1"]
sc('generated batch-get debug redaction', [*BG, '--debug'], responses=[BGBODY], masks=DEBUG_MASKS,
   stderr_records=' - botocore\\.parsers - ')
sc('generated list debug redaction of the preset token', [*LDR, *EPA, '--debug'],
   env={'AWS_BEARER_TOKEN_AGENT_REGISTRY': 'preset-token-0123456789'},
   responses=[resp({'registryRecords': [], 'nextToken': 'preset-token-0123456789 eyJhbGci.eyJzdWIi.sig'})], masks=DEBUG_MASKS,
   stderr_records=' - botocore\\.parsers - ')
HIST_CONFIG = '[default]\nregion = us-east-1\ncli_history = enabled\n'
HIST_MASKS = [UUID, "[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}(\\.[0-9]{3}| [AP]M)",
              "\"Authorization\": \"[^\"]*\"", "\"X-Amz-Date\": \"[^\"]*\"", "\"User-Agent\": \"[^\"]*\"",
              "with AWS CLI version: [^\\n]*", "\"amz-sdk-invocation-id\": \"[^\"]*\"", "\"Date\": \"[^\"]*\""]
sc('history batch-get redaction', ['history', 'show'], setup=[BG], files={'home/.aws/config': HIST_CONFIG},
   responses=[BGBODY], masks=HIST_MASKS)
sc('history list-skills redaction', ['history', 'show'], setup=[ar('list-skills', '--registry-id', REG, *EPA)],
   files={'home/.aws/config': HIST_CONFIG}, responses=[page(P1), BGBODY], masks=HIST_MASKS)

json.dump(S, open(OUT, 'w'), indent=1, ensure_ascii=False)
open(OUT, 'a').write('\n')
print(len(S), 'scenarios')
