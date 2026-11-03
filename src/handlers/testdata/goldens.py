"""Print src/handlers/testdata/goldens.jsonl (one golden per line) from the
reference botocore and CLI handlers."""
import base64
import datetime
import hashlib
import io
import json
from collections import OrderedDict
from urllib.parse import urlencode

import awscli  # noqa
from botocore import auth, handlers, utils
from botocore import session as bs
from botocore.exceptions import ParamValidationError
from awscli.customizations import s3errormsg
from awscli.customizations.datapipeline import translator

out = {}

def body(n):
    return bytes((i * 7 + 3) % 251 for i in range(n))

out['tree_hash'] = [[n, utils.calculate_tree_hash(io.BytesIO(body(n)))] for n in [0, 5, 1024 * 1024, 1024 * 1024 + 1, 3 * 1024 * 1024 + 7]]

p = {'SSECustomerKey': '/path/to/thing', 'CopySourceSSECustomerKey': 'ü key'}
handlers.sse_md5(p)
handlers.copy_source_sse_md5(p)
out['sse_md5'] = p

out['copy_source'] = [[v, handlers._quote_source_header(v)] for v in [
    'bucket/key with space', 'b/ü~k+x', 'b/k?versionId=abc', 'b/k?versionId=a?versionId=b', 'b/k?versionId=a b', 'b/k?versionId=abc\n', 'b/a%2Fb']]
cs = {'CopySource': {'Bucket': 'arn:aws:s3:us-west-2:123456789012:accesspoint/ap', 'Key': 'k y', 'VersionId': 'v1'}}
handlers.handle_copy_source_param(cs)
out['copy_source_dict'] = cs['CopySource']

from urllib.parse import unquote
out['unquote'] = [[v, unquote(v)] for v in ['a%20b', '%E3%81%82', '%E3%81', '%zz%4', 'ü%41', '%ff%41', 'plain+x']]

try:
    handlers.validate_bucket_name({'Bucket': 'bad bucket!'})
except ParamValidationError as e:
    out['bucket_error'] = str(e)
out['bucket_ok'] = []
for b in ['ok.bucket-1_x', 'ok\n', 'arn:aws:s3:us-west-2:123456789012:accesspoint/ap', 'a' * 256]:
    try:
        handlers.validate_bucket_name({'Bucket': b})
        out['bucket_ok'].append([b, True])
    except ParamValidationError:
        out['bucket_ok'].append([b, False])

try:
    handlers.validate_ascii_metadata({'Metadata': {'k': 'vé'}})
except ParamValidationError as e:
    out['ascii_error'] = str(e)

s = bs.get_session()
iam = s.get_service_model('iam')
parsed = {'Policy': {'PolicyName': 'p', 'DefaultVersionId': 'v1'}, 'ResponseMetadata': {}}
op = iam.operation_model('GetRolePolicy')
parsed = {'RoleName': 'r', 'PolicyName': 'p', 'PolicyDocument': '%7B%22Version%22%3A%20%222012-10-17%22%2C%20%22N%22%3A%201.50%7D'}
handlers.json_decode_policies(parsed, op)
out['iam_get_role_policy'] = parsed
op = iam.operation_model('GetAccountAuthorizationDetails')
parsed = {'RoleDetailList': [{'RoleName': 'r', 'AssumeRolePolicyDocument': '%5B1%2C%202%5D', 'RolePolicyList': [{'PolicyName': 'a', 'PolicyDocument': 'not%20json'}]}]}
handlers.json_decode_policies(parsed, op)
out['iam_details'] = parsed

parsed = {'InstanceId': 'i', 'Output': base64.b64encode('héllo\n'.encode() + b'\xff').decode()}
handlers.decode_console_output(parsed)
out['console_output'] = parsed
parsed = {'Output': 'aGk'}
handlers.decode_console_output(parsed)
out['console_output_bad'] = parsed

parsed = {'TemplateBody': '{"b": 1, "a": [1.0, "x"]}', 'StagesAvailable': []}
handlers.json_decode_template_body(parsed)
out['template'] = parsed

d = {'pipelineObjects': [{'id': 'Default', 'name': 'Default', 'fields': [{'key': 'a', 'stringValue': '1'}, {'key': 'a', 'refValue': 'x'}, {'key': 'a', 'stringValue': '2'}, {'key': 'b', 'refValue': 'y'}]}], 'parameterObjects': [{'id': 'p', 'attributes': [{'key': 't', 'stringValue': 'String'}]}], 'parameterValues': [{'id': 'v', 'stringValue': '1'}, {'id': 'v', 'stringValue': '2'}], 'ResponseMetadata': {}}
translator.api_to_definition(d)
out['datapipeline'] = d

errs = []
for e in [
    {'Error': {'Code': 'X', 'Message': 'Please use AWS4-HMAC-SHA256.'}},
    {'Error': {'Code': 'PermanentRedirect', 'Message': 'The bucket must be addressed.', 'Endpoint': 'b.s3.amazonaws.com'}},
    {'Error': {'Code': 'Y', 'Message': 'AWS KMS managed keys require AWS Signature Version 4'}},
]:
    s3errormsg.enhance_error_msg(e)
    errs.append(e)
out['s3errormsg'] = errs

parsed = {'EncodingType': 'url', 'Prefix': 'a%20b', 'Contents': [{'Key': 'k%2By'}], 'CommonPrefixes': [{'Prefix': 'p%2F'}]}
handlers.decode_list_object_v2(parsed, {'encoding_type_auto_set': True})
out['list_v2'] = parsed

class FakeDT(datetime.datetime):
    @classmethod
    def utcnow(cls):
        return datetime.datetime(2026, 11, 3, 12, 34, 56)

class FakeMod:
    datetime = FakeDT
auth.datetime = FakeMod

from botocore.credentials import Credentials
def presign(service, region, body_dict, handler, token=None):
    client = s.create_client(service, region_name=region, aws_access_key_id='AKID', aws_secret_access_key='SECRET', aws_session_token=token)
    signer = client._request_signer
    model = client.meta.service_model.operation_model(body_dict['Action'])
    before = urlencode(list(body_dict.items()))
    rd = {'url_path': '/', 'query_string': '', 'method': 'POST', 'headers': {}, 'body': dict(body_dict), 'url': f'https://{service}.{region}.amazonaws.com/', 'context': {}}
    handler(params=rd, request_signer=signer, model=model)
    return [before, urlencode(list(rd['body'].items()))]

out['presign_ec2'] = presign('ec2', 'us-east-1', OrderedDict([('Action', 'CopySnapshot'), ('Version', '2016-11-15'), ('Description', 'a b/c~'), ('SourceRegion', 'us-west-2'), ('SourceSnapshotId', 'snap-1')]), handlers.inject_presigned_url_ec2)
out['presign_rds'] = presign('rds', 'eu-west-1', OrderedDict([('Action', 'CopyDBSnapshot'), ('Version', '2014-10-31'), ('SourceDBSnapshotIdentifier', 'arn:aws:rds:us-east-1:1:snapshot:s'), ('TargetDBSnapshotIdentifier', 't'), ('SourceRegion', 'us-east-1')]), handlers.inject_presigned_url_rds, token='TOKEN')

for name, value in out.items():
    print(json.dumps({"name": name, "value": value, "note": "gitleaks:allow"}, ensure_ascii=False))
