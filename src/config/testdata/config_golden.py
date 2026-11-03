"""Print configuration goldens from the reference CLI's session (one JSON
object per line: the scenario and the resolved values).

PYTHONPATH=$AWSR_REFERENCE_SRC python3 config_golden.py > config.jsonl

Each scenario runs in a subprocess with only HOME (a temporary directory
holding the scenario's ~/.aws files) and the scenario's variables in the
environment, through `awscli.clidriver.create_clidriver` with the
`--profile` / `--region` instance variables set the way
`CLIDriver._handle_top_level_args` sets them.
"""
import json
import os
import subprocess
import sys
import tempfile

NAMES = [
    'profile', 'region', 'output', 'cli_binary_format', 'cli_error_format',
    'cli_auto_prompt', 'pager', 'retry_mode', 'max_attempts', 'ca_bundle',
    'endpoint_discovery_enabled', 'use_fips_endpoint', 'use_dualstack_endpoint',
    'sigv4a_signing_region_set', 'disable_host_prefix_injection',
    'user_agent_appid', 'metadata_service_timeout', 'parameter_validation',
    'request_checksum_calculation', 'auth_scheme_preference', 's3',
]

SCENARIOS = [
    dict(name='empty', config=None, credentials=None, env={}, profile=None, region=None),
    dict(name='default_profile',
         config='[default]\nregion = us-west-2\noutput = text\ncli_binary_format = raw-in-base64-out\n'
                'max_attempts = 5\nretry_mode = adaptive\ns3 =\n  addressing_style = path\n'
                '  payload_signing_enabled = TRUE\nuse_fips_endpoint = True\n',
         credentials='[default]\naws_access_key_id = a\n', env={}, profile=None, region=None),
    dict(name='region_precedence',
         config='[default]\nregion = from-config\n', credentials=None,
         env={'AWS_REGION': 'from-aws-region', 'AWS_DEFAULT_REGION': 'from-default'},
         profile=None, region=None),
    dict(name='default_region_env',
         config='[default]\nregion = from-config\n', credentials=None,
         env={'AWS_DEFAULT_REGION': 'from-default', 'AWS_DEFAULT_OUTPUT': 'yaml',
              'AWS_MAX_ATTEMPTS': '7', 'AWS_RETRY_MODE': 'standard', 'AWS_CA_BUNDLE': '/x.pem',
              'AWS_PAGER': '', 'AWS_CLI_ERROR_FORMAT': 'JSON', 'AWS_SDK_UA_APP_ID': 'myapp',
              'AWS_USE_DUALSTACK_ENDPOINT': 'yes'},
         profile=None, region=None),
    dict(name='region_arg',
         config='[default]\nregion = from-config\n', credentials=None,
         env={'AWS_REGION': 'from-aws-region'}, profile=None, region='from-arg'),
    dict(name='named_profile_env',
         config='[default]\nregion = d\n[profile dev]\nregion = eu-west-1\noutput = table\n'
                'cli_pager = cat\n', credentials='[dev]\nregion = ignored\nmetadata_service_timeout = 4\n',
         env={'AWS_PROFILE': 'dev', 'AWS_DEFAULT_PROFILE': 'other', 'PAGER': 'more'},
         profile=None, region=None),
    dict(name='profile_arg_wins',
         config='[profile dev]\nregion = eu-west-1\n[profile prod]\nregion = ap-northeast-1\n',
         credentials=None, env={'AWS_PROFILE': 'dev'}, profile='prod', region=None),
    dict(name='credentials_only_profile',
         config=None, credentials='[ci]\nregion = us-east-2\noutput = json\n',
         env={}, profile='ci', region=None),
    dict(name='missing_profile',
         config='[default]\nregion = us-east-1\n', credentials=None, env={},
         profile='nope', region=None),
    dict(name='missing_profile_env',
         config='[default]\nregion = us-east-1\n', credentials=None,
         env={'AWS_PROFILE': 'nope', 'AWS_REGION': 'r'}, profile=None, region=None),
    dict(name='quoted_profile_and_default_section',
         config='[DEFAULT]\noutput = text\n[profile "my dev"]\nregion = sa-east-1\n',
         credentials=None, env={}, profile='my dev', region=None),
    dict(name='custom_paths',
         config=None, credentials=None,
         env={'AWS_CONFIG_FILE': '$HOME/other/config', 'AWS_SHARED_CREDENTIALS_FILE': '~/other/creds'},
         files={'other/config': '[default]\nregion = from-custom\n',
                'other/creds': '[default]\noutput = text\n'},
         profile=None, region=None),
    dict(name='comments_and_continuations',
         config='# top\n[default]\n; c\nregion = us-west-1\nsigv4a_signing_region_set = us-west-1,\n'
                '  us-east-1\nauth_scheme_preference=sigv4a ,sigv4\n'
                'disable_host_prefix_injection = true\n',
         credentials=None, env={}, profile=None, region=None),
]


def run_one(scenario):
    with tempfile.TemporaryDirectory() as home:
        os.makedirs(os.path.join(home, '.aws'))
        if scenario['config'] is not None:
            with open(os.path.join(home, '.aws', 'config'), 'w') as f:
                f.write(scenario['config'])
        if scenario['credentials'] is not None:
            with open(os.path.join(home, '.aws', 'credentials'), 'w') as f:
                f.write(scenario['credentials'])
        for rel, text in scenario.get('files', {}).items():
            path = os.path.join(home, rel)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, 'w') as f:
                f.write(text)
        env = {'HOME': home, 'PYTHONPATH': os.environ.get('PYTHONPATH', ''),
               'AWS_EC2_METADATA_DISABLED': 'true'}
        env.update(scenario['env'])
        out = subprocess.run(
            [sys.executable, __file__, '--child', json.dumps(scenario)],
            env=env, capture_output=True, text=True, check=True,
        ).stdout
        return json.loads(out)


def child(scenario):
    from awscli.clidriver import create_clidriver

    driver = create_clidriver()
    session = driver.session
    if scenario['profile']:
        session.set_config_variable('profile', scenario['profile'])
    if scenario['region']:
        session.set_config_variable('region', scenario['region'])
    values = {}
    for name in NAMES:
        try:
            v = session.get_config_variable(name)
        except Exception as e:  # noqa: BLE001
            v = {'error': str(e)}
        if name == 'pager' and v == 'less':
            v = 'less'
        values[name] = v
    try:
        values['cli_timestamp_format'] = session.get_scoped_config().get(
            'cli_timestamp_format', 'iso8601')
    except Exception as e:  # noqa: BLE001
        values['cli_timestamp_format'] = {'error': str(e)}
    print(json.dumps(values))


if __name__ == '__main__':
    if len(sys.argv) > 2 and sys.argv[1] == '--child':
        child(json.loads(sys.argv[2]))
    else:
        for s in SCENARIOS:
            print(json.dumps(dict(s, expected=run_one(s))))
