"""Print SigV4 goldens from botocore's SigV4Auth / S3SigV4Auth with a fixed
clock (one JSON object per line: the inputs and the signed headers).

PYTHONPATH=$AWSR_REFERENCE_SRC python3 sigv4_golden.py > sigv4.jsonl
"""
import awscli  # noqa
import datetime
import json
from unittest import mock

from botocore.auth import S3SigV4Auth, SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials

NOW = datetime.datetime(2026, 11, 3, 12, 0, 0)

CASES = [
    dict(name='sts_post', cls='v4', service='sts', region='us-west-2', token=None,
         method='POST', url='http://127.0.0.1:18081/',
         headers=[['Content-Type', 'application/x-www-form-urlencoded; charset=utf-8'], ['User-Agent', 'ua']],
         body='Action=GetCallerIdentity&Version=2011-06-15', ctx={}),
    dict(name='sqs_json_token', cls='v4', service='sqs', region='us-east-1', token='tok',
         method='POST', url='https://sqs.us-east-1.amazonaws.com/',
         headers=[['X-Amz-Target', 'AmazonSQS.ListQueues'], ['Content-Type', 'application/x-amz-json-1.0'],
                  ['x-amzn-query-mode', 'true'], ['User-Agent', 'ua'], ['Expect', '100-continue']],
         body='{"QueueNamePrefix": "a b+c/~"}', ctx={}),
    dict(name='path_normalize', cls='v4', service='execute-api', region='eu-west-1', token=None,
         method='GET', url='https://h.example.com:8443/a/./b/../c//d%2Fe/%7Ex?b=2&a=1&a=0&c=&d',
         headers=[['X-Spaces', '  a   b  c '], ['x-dup', 'one'], ['X-Dup', 'two']],
         body='', ctx={}),
    dict(name='date_header', cls='v4', service='iam', region='us-east-1', token='t2',
         method='POST', url='https://iam.amazonaws.com',
         headers=[['Date', 'whatever']], body='x=1', ctx={}),
    dict(name='unsigned_body', cls='v4', service='lambda', region='us-east-1', token=None,
         method='PUT', url='https://lambda.us-east-1.amazonaws.com/x',
         headers=[], body='abc', ctx={'payload_signing_enabled': False}),
    dict(name='s3_get', cls='s3v4', service='s3', region='us-west-2', token=None,
         method='GET', url='http://127.0.0.1:18081/bkt/a%20b/./c?list-type=2&prefix=a%20b%2Bc%2F~%2A&encoding-type=url',
         headers=[['User-Agent', 'ua']], body='', ctx={}),
    dict(name='s3_put_https_md5_streaming', cls='s3v4', service='s3', region='us-east-1', token='tok',
         method='PUT', url='https://bkt.s3.amazonaws.com/key',
         headers=[['Content-MD5', 'abc=='], ['Expect', '100-continue']], body='data',
         ctx={'has_streaming_input': True}),
    dict(name='s3_put_https_plain', cls='s3v4', service='s3', region='us-east-1', token=None,
         method='PUT', url='https://bkt.s3.amazonaws.com/key',
         headers=[], body='data', ctx={'has_streaming_input': True}),
]

for case in CASES:
    creds = Credentials('AKIDEXAMPLE', 'wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY', case['token'])
    cls = SigV4Auth if case['cls'] == 'v4' else S3SigV4Auth
    auth = cls(creds, case['service'], case['region'])
    req = AWSRequest(method=case['method'], url=case['url'], data=case['body'].encode())
    for k, v in case['headers']:
        req.headers[k] = v
    req.context.update(case['ctx'])
    with mock.patch('botocore.auth.datetime') as dt:
        dt.datetime.utcnow.return_value = NOW
        dt.datetime.strptime = datetime.datetime.strptime
        auth.add_auth(req)
    # The credentials are fake; the marker keeps secret scanners quiet.
    out = dict(case, signed=list(req.headers.items()), note='gitleaks:allow')
    print(json.dumps(out))
