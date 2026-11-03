"""Goldens for presign.rs and eks.rs: botocore presigned URLs with the
signing time fixed to 2026-01-02T03:04:05Z.

PYTHONPATH=$AWSR_REFERENCE_SRC python3 gen_presign.py <outdir>
"""
import datetime
import os
import sys

import awscli  # noqa
import botocore.auth as _auth
import botocore.session as _bsession

class Fixed(datetime.datetime):
    @classmethod
    def utcnow(cls):
        return datetime.datetime(2026, 1, 2, 3, 4, 5)

_auth.datetime.datetime = Fixed
out = sys.argv[1]
SECRET = 'wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY'

def client(service, region, token=None):
    s = _bsession.get_session()
    return s.create_client(
        service,
        region_name=region,
        aws_access_key_id='AKIDEXAMPLE',
        aws_secret_access_key=SECRET,
        aws_session_token=token,
    )

def write(name, text):
    with open(os.path.join(out, name), 'w') as f:
        f.write(text + '\n')

rds = client('rds', 'us-east-1')
write('rds_token.txt', 'https://' + rds.generate_db_auth_token('db.example.com', 5432, 'admin user'))
rds = client('rds', 'us-west-2', 'tok/en+=')
write('rds_token_session.txt', 'https://' + rds.generate_db_auth_token('db.example.com', 5432, 'admin user'))
dsql = client('dsql', 'us-east-1')
write('dsql_token.txt', 'https://' + dsql.generate_db_connect_auth_token('abc.dsql.us-east-1.on.aws', 'us-east-1', 900))
write('dsql_admin_token.txt', 'https://' + dsql.generate_db_connect_admin_auth_token('abc.dsql.us-east-1.on.aws', 'us-east-1', 3600))

from awscli.customizations.eks.get_token import STSClientFactory, TokenGenerator  # noqa: E402

for name, token, region in [('eks_token.txt', None, 'us-east-1'), ('eks_token_session.txt', 'tok', 'eu-west-1')]:
    s = _bsession.get_session()
    s.set_credentials('AKIDEXAMPLE', SECRET, token)
    sts = STSClientFactory(s).get_sts_client(region_name=region)
    gen = TokenGenerator(sts)
    write(name, gen._get_presigned_url('my-cluster') + '\n' + gen.get_token('my-cluster'))

import awscli.customizations.codecommit as _cc  # noqa: E402

_cc.datetime.datetime = Fixed
s = _bsession.get_session()
s.set_credentials('AKIDEXAMPLE', SECRET)
write('codecommit_signature.txt', _cc.CodeCommitGetCommand(s).sign_request(
    'eu-west-1', 'https://git-codecommit.eu-west-1.amazonaws.com/v1/repos/r'))
