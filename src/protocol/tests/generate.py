"""Generate src/protocol/tests/goldens.rs from the reference botocore.

    TZ=UTC PYTHONPATH=$AWSR_REFERENCE_SRC python3 src/protocol/tests/generate.py \
        > src/protocol/tests/goldens.rs

Each serialization case feeds the same params to botocore's serializer and
records the request dict; each parsing case feeds a synthetic HTTP response
to the protocol parser, then applies the steps of
`Endpoint._do_get_response` (modeled error fields, `RetryAttempts`), and
records a type- and order-preserving dump of the result.
"""

import io
import json
import struct
import sys

import awscli  # noqa: F401  (makes the vendored botocore importable as `botocore`)
from urllib3._collections import HTTPHeaderDict

import botocore.session
from botocore import serialize
from botocore.awsrequest import HeadersDict
from botocore.parsers import ResponseParserFactory
from botocore.response import StreamingBody

from awscli.customizations.timestampformat import iso_format


def identity(x):
    return x


SESSION = botocore.session.get_session()
MODELS = {}


def model(name):
    if name not in MODELS:
        MODELS[name] = SESSION.get_service_model(name)
    return MODELS[name]


def dump(v):
    if v is None:
        return 'None'
    if isinstance(v, bool):
        return 'True' if v else 'False'
    if isinstance(v, int):
        return str(v)
    if isinstance(v, float):
        return 'f%016x' % struct.unpack('>Q', struct.pack('>d', v))[0]
    if isinstance(v, str):
        return 's' + json.dumps(v)
    if isinstance(v, (bytes, bytearray)):
        return "b'" + bytes(v).hex() + "'"
    if isinstance(v, StreamingBody):
        return dump(v.read())
    if isinstance(v, list):
        return '[' + ', '.join(dump(x) for x in v) + ']'
    if isinstance(v, dict):
        return '{' + ', '.join(f'{dump(k)}: {dump(x)}' for k, x in v.items()) + '}'
    raise TypeError(type(v))


def rust_str(s):
    out = ['"']
    for c in s:
        if c == '"':
            out.append('\\"')
        elif c == '\\':
            out.append('\\\\')
        elif c == '\n':
            out.append('\\n')
        elif ' ' <= c <= '~':
            out.append(c)
        else:
            out.append('\\u{%x}' % ord(c))
    out.append('"')
    return ''.join(out)


def rust_bytes(b):
    out = ['b"']
    for x in b:
        c = chr(x)
        if c == '"':
            out.append('\\"')
        elif c == '\\':
            out.append('\\\\')
        elif ' ' <= c <= '~':
            out.append(c)
        else:
            out.append('\\x%02x' % x)
    out.append('"')
    return ''.join(out)


# --- a small CBOR encoder for synthetic responses -------------------------


class Tag1:
    def __init__(self, v):
        self.v = v


class Half:
    def __init__(self, bits):
        self.bits = bits


class Single:
    def __init__(self, v):
        self.v = v


class Indef:
    def __init__(self, v):
        self.v = v


def head(major, n):
    m = major << 5
    if n < 24:
        return bytes([m | n])
    if n < 256:
        return bytes([m | 24, n])
    if n < 65536:
        return bytes([m | 25]) + n.to_bytes(2, 'big')
    if n < 2**32:
        return bytes([m | 26]) + n.to_bytes(4, 'big')
    return bytes([m | 27]) + n.to_bytes(8, 'big')


def cbor(v):
    if v is None:
        return b'\xf6'
    if v is True:
        return b'\xf5'
    if v is False:
        return b'\xf4'
    if isinstance(v, int):
        return head(0, v) if v >= 0 else head(1, -1 - v)
    if isinstance(v, float):
        return b'\xfb' + struct.pack('>d', v)
    if isinstance(v, Half):
        return b'\xf9' + struct.pack('>H', v.bits)
    if isinstance(v, Single):
        return b'\xfa' + struct.pack('>f', v.v)
    if isinstance(v, Tag1):
        return b'\xc1' + cbor(v.v)
    if isinstance(v, str):
        e = v.encode()
        return head(3, len(e)) + e
    if isinstance(v, bytes):
        return head(2, len(v)) + v
    if isinstance(v, Indef):
        x = v.v
        if isinstance(x, list):
            return b'\x9f' + b''.join(cbor(i) for i in x) + b'\xff'
        if isinstance(x, dict):
            return b'\xbf' + b''.join(cbor(k) + cbor(i) for k, i in x.items()) + b'\xff'
        if isinstance(x, tuple):
            # chunked text string
            return b'\x7f' + b''.join(cbor(i) for i in x) + b'\xff'
    if isinstance(v, list):
        return head(4, len(v)) + b''.join(cbor(i) for i in v)
    if isinstance(v, dict):
        return head(5, len(v)) + b''.join(cbor(k) + cbor(i) for k, i in v.items())
    raise TypeError(type(v))


# --- serialization --------------------------------------------------------

SER = [
    # json
    ('dynamodb', 'PutItem', {
        'TableName': 'tbl',
        'Item': {
            'id': {'S': 'café \U0001f600'},
            'n': {'N': '5'},
            'b': {'B': 'aGVsbG8='},
            'l': {'L': [{'BOOL': True}, {'NULL': True}]},
            'm': {'M': {'x': {'SS': ['a', 'b']}, 'bs': {'BS': ['AQI=']}}},
        },
        'ReturnValues': 'ALL_OLD',
        'ConditionExpression': 'attribute_not_exists(id)',
    }),
    ('dynamodb', 'ListTables', {}),
    ('dynamodb', 'Query', {
        'TableName': 't', 'Limit': 10, 'ScanIndexForward': False,
        'KeyConditionExpression': '#k = :v',
        'ExpressionAttributeNames': {'#k': 'pk'},
        'ExpressionAttributeValues': {':v': {'S': 'x'}},
        'ExclusiveStartKey': {'pk': {'S': 'a'}},
    }),
    ('kms', 'Encrypt', {
        'KeyId': 'alias/my-key', 'Plaintext': 'secret\n', 'EncryptionContext': {'b': '2', 'a': '1'},
        'GrantTokens': ['t1', 't2'], 'DryRun': True,
    }),
    ('logs', 'GetLogEvents', {
        'logGroupName': '/aws/lambda/f', 'logStreamName': '2023/01/01/[$LATEST]abc',
        'startTime': 1700000000000, 'startFromHead': True, 'limit': 100,
    }),
    ('logs', 'PutLogEvents', {
        'logGroupName': 'g', 'logStreamName': 's',
        'logEvents': [{'timestamp': 1, 'message': 'a "quoted" \\ line\ttab'}, {'timestamp': 2, 'message': '日本'}],
    }),
    ('ssm', 'GetParameters', {'Names': ['/a/b', '/c'], 'WithDecryption': True}),
    ('ssm', 'PutParameter', {'Name': '/x', 'Value': 'v', 'Type': 'SecureString', 'Overwrite': False, 'Tags': [{'Key': 'k', 'Value': 'v'}]}),
    ('ecs', 'RunTask', {
        'cluster': 'c', 'taskDefinition': 'td:1', 'count': 2,
        'overrides': {'containerOverrides': [{'name': 'app', 'command': ['echo', 'hi'], 'environment': [{'name': 'A', 'value': '1'}], 'cpu': 256, 'memory': 512}]},
        'networkConfiguration': {'awsvpcConfiguration': {'subnets': ['s-1'], 'assignPublicIp': 'ENABLED'}},
        'enableExecuteCommand': True,
    }),
    ('cloudwatch', 'GetMetricStatistics', {
        'Namespace': 'AWS/EC2', 'MetricName': 'CPUUtilization',
        'Dimensions': [{'Name': 'InstanceId', 'Value': 'i-1'}],
        'StartTime': '2023-01-01T00:00:00Z', 'EndTime': 1700000000.5, 'Period': 60,
        'Statistics': ['Average', 'Maximum'],
    }),
    ('cloudwatch', 'PutMetricData', {
        'Namespace': 'ns',
        'MetricData': [
            {'MetricName': 'm', 'Value': 1.5, 'Timestamp': '2023-01-01T09:00:00.25+09:00', 'Values': [1, 2.5, 1e-7, 1e16], 'Counts': [1.0, 2.0], 'StorageResolution': 1},
            {'MetricName': 'n', 'Value': float('nan'), 'Timestamp': '1700000000'},
            {'MetricName': 'o', 'Value': float('inf'), 'Timestamp': 1700000000},
            {'MetricName': 'p', 'Value': float('-inf'), 'Timestamp': '2023-01-01'},
        ],
    }),
    ('sqs', 'SendMessage', {
        'QueueUrl': 'https://sqs.us-east-1.amazonaws.com/123/q', 'MessageBody': 'hello',
        'DelaySeconds': 0,
        'MessageAttributes': {'a': {'DataType': 'Binary', 'BinaryValue': '\x00\x01'}, 's': {'DataType': 'String', 'StringValue': 'x'}},
    }),
    ('servicediscovery', 'DiscoverInstances', {'NamespaceName': 'ns', 'ServiceName': 'svc', 'QueryParameters': {'k': 'v'}}),
    # rest-json
    ('lambda', 'Invoke', {
        'FunctionName': 'my-fn', 'InvocationType': 'RequestResponse', 'LogType': 'Tail',
        'ClientContext': 'e30=', 'Payload': '{"a": 1}', 'Qualifier': '$LATEST',
    }),
    ('lambda', 'Invoke', {'FunctionName': 'arn:aws:lambda:us-east-1:123456789012:function:f'}),
    ('lambda', 'ListFunctions', {'MaxItems': 10, 'Marker': 'a b/c', 'FunctionVersion': 'ALL', 'MasterRegion': 'us-east-1'}),
    ('lambda', 'GetFunction', {'FunctionName': 'f', 'Qualifier': '1'}),
    ('lambda', 'CreateFunction', {
        'FunctionName': 'f', 'Runtime': 'python3.12', 'Role': 'arn:aws:iam::1:role/r', 'Handler': 'h.h',
        'Code': {'ZipFile': 'PK\x03\x04'}, 'Timeout': 30, 'MemorySize': 128, 'Publish': True,
        'Environment': {'Variables': {'B': '2', 'A': '1'}}, 'Tags': {'t': 'v'},
        'Architectures': ['arm64'],
    }),
    ('lambda', 'ListTags', {'Resource': 'arn:aws:lambda:us-east-1:1:function:f'}),
    ('lambda', 'DeleteFunction', {'FunctionName': 'f'}),
    ('apigateway', 'GetResources', {'restApiId': 'abc', 'limit': 5, 'embed': ['methods', 'x y']}),
    ('apigateway', 'GetExport', {'restApiId': 'a', 'stageName': 's', 'exportType': 'swagger', 'parameters': {'extensions': 'integrations', 'z': 'a&b'}, 'accepts': 'application/json'}),
    ('apigateway', 'PutIntegration', {
        'restApiId': 'a', 'resourceId': 'r', 'httpMethod': 'GET', 'type': 'HTTP',
        'requestParameters': {'integration.request.path.id': 'method.request.path.id'},
        'timeoutInMillis': 29000, 'cacheKeyParameters': ['a'],
    }),
    ('iot-data', 'Publish', {
        'topic': 'a/b c/#', 'qos': 1, 'retain': True, 'payload': 'hello',
        'userProperties': '[{"deviceName": "alpha"}]', 'messageExpiry': 30,
    }),
    ('iot', 'ListThings', {'maxResults': 5, 'attributeName': 'a', 'attributeValue': 'b', 'usePrefixAttributeValue': False}),
    ('iot', 'CreateThing', {'thingName': 't', 'attributePayload': {'attributes': {'k': 'v'}, 'merge': True}}),
    ('iot', 'GetTopicRuleDestination', {'arn': 'arn:aws:iot:us-east-1:1:ruledestination/http/a b~c'}),
    ('efs', 'CreateFileSystem', {
        'CreationToken': 'tok', 'PerformanceMode': 'generalPurpose', 'Encrypted': True,
        'ThroughputMode': 'provisioned', 'ProvisionedThroughputInMibps': 1.5,
        'Tags': [{'Key': 'Name', 'Value': 'fs'}],
    }),
    ('efs', 'DescribeFileSystems', {'MaxItems': 1, 'FileSystemId': 'fs-1'}),
    ('amp', 'ListScrapers', {'filters': {'status': ['ACTIVE', 'CREATING'], 'alias': ['x y']}, 'maxResults': 5}),
    ('accessanalyzer', 'UpdateAnalyzer', {'analyzerName': 'a', 'configuration': {'unusedAccess': {'unusedAccessAge': 90}}}),
    ('cognito-idp', 'UpdateManagedLoginBranding', {
        'ManagedLoginBrandingId': 'i', 'UserPoolId': 'p',
        'Settings': {'a': [1, 2.5, None, True], 'b': {'c': 'é'}, 'n': float('nan')},
    }),
    ('appsync', 'ListTagsForResource', {'resourceArn': 'arn:aws:appsync:us-east-1:1:apis/x'}),
    ('appsync', 'GetIntrospectionSchema', {'apiId': 'a', 'format': 'SDL', 'includeDirectives': True}),
    ('mediastore-data', 'GetObject', {'Path': 'folder/sub dir/file~1.txt', 'Range': 'bytes=0-9'}),
    ('mediastore-data', 'PutObject', {'Path': '/a/b', 'Body': 'data', 'ContentType': 'text/plain', 'CacheControl': 'no-cache'}),
    ('connect', 'CreateTestCase', {'InstanceId': 'i', 'Name': 'n', 'Content': 'c', 'LastModifiedTime': '2023-01-01T00:00:00.5Z'}),
    ('dataexchange', 'SendApiAsset', {'AssetId': 'a', 'DataSetId': 'd', 'RevisionId': 'r', 'Body': 'b', 'RequestHeaders': {'X-Foo': 'bar'}, 'Method': 'POST', 'Path': '/p', 'QueryStringParameters': {'q': 'v'}}),
    ('groundstation', 'ListGroundStationReservations', {'groundStationId': 'g', 'startTime': '2023-01-01T00:00:00Z', 'endTime': 1700000000.25}),
    ('emr-containers', 'ListJobRuns', {'virtualClusterId': 'v', 'createdBefore': 1700000000, 'createdAfter': '2023-01-01T00:00:00.123+01:00', 'states': ['RUNNING', 'FAILED']}),
    ('polly', 'StartSpeechSynthesisStream', {'Engine': 'generative', 'LexiconNames': ['a,b', 'c"d', 'e'], 'OutputFormat': 'mp3', 'VoiceId': 'Joanna', 'ActionStream': {}}),
    ('neptune-graph', 'ExecuteQuery', {'graphIdentifier': 'g-123', 'queryString': 'MATCH (n) RETURN n', 'language': 'OPEN_CYPHER'}),
    ('bedrock-agent', 'GetKnowledgeBase', {'knowledgeBaseId': 'kb'}),
    ('lex-runtime', 'PostContent', {
        'botName': 'b', 'botAlias': 'a', 'userId': 'u', 'contentType': 'text/plain; charset=utf-8',
        'inputStream': 'hi', 'sessionAttributes': {'k': 'v'}, 'accept': 'text/plain; charset=utf-8',
    }),
    # smithy-rpc-v2-cbor
    ('eventbridgev2', 'CreateEventBus', {
        'Name': 'bus', 'Description': 'd' * 30, 'StorageConfiguration': {'RetentionPeriodInDays': 300},
        'Tags': {'a': 'b'}, 'ClientToken': 'tok',
    }),
    ('eventbridgev2', 'PutEvents', {
        'EventBusArn': 'arn', 'Entries': [
            {'Source': 's', 'DetailType': 't', 'Detail': '{}', 'Resources': ['r1', 'r2'], 'Time': '2023-01-01T00:00:00.5Z'},
            {'Source': 's2', 'Time': 1700000000},
        ],
    }),
    ('eventbridgev2', 'PutRawEvents', {'EventBusArn': 'arn', 'Entries': [{'Data': 'x' * 300, 'Metadata': {'k': 'v'}}]}),
    ('eventbridgev2', 'ListEventBuses', {}),
    ('cloudwatchomni', 'GetContextGraph', {
        'nodeFilters': {'region': ['us-east-1'], 'tags': [{'key': 'k', 'values': ['v']}]},
        'startTime': 1700000000.123, 'endTime': '2023-11-15T00:00:00Z', 'depth': -3, 'maxResults': 70000,
    }),
    ('cloudwatchomni', 'CreateAlert', {
        'spaceId': 's', 'profileId': 'p', 'name': 'n', 'notificationsEnabled': False,
        'rule': {'telemetryRule': {'condition': {'warningThreshold': 1.5, 'criticalThreshold': 100}}},
    }),
    ('cloudwatchomni', 'CreateAlert', {
        'spaceId': 's', 'profileId': 'p', 'name': 'n' * 300,
        'rule': {'telemetryRule': {'condition': {'warningThreshold': float('nan'), 'criticalThreshold': float('-inf')}}},
    }),
]


def ser_case(svc, op, params):
    m = model(svc)
    om = m.operation_model(op)
    s = serialize.create_serializer(m.resolved_protocol, include_validation=False)
    r = s.serialize_to_request(params, om)
    qs = r['query_string']
    pairs = []
    if isinstance(qs, dict):
        for k, v in qs.items():
            if isinstance(v, list):
                pairs.extend((k, str(e)) for e in v)
            else:
                pairs.append((k, str(v)))
    headers = [(k, str(v)) for k, v in r['headers'].items()]
    body = r['body']
    if isinstance(body, str):
        body = body.encode()
    return {
        'service': svc, 'op': op, 'params': params,
        'method': r['method'], 'url_path': r['url_path'], 'query': pairs,
        'headers': headers, 'body': body, 'host_prefix': r.get('host_prefix'),
    }


# --- parsing --------------------------------------------------------------

J = lambda v: json.dumps(v).encode()  # noqa: E731
RID = [('x-amzn-RequestId', 'req-1')]

PARSE = [
    # json
    ('dynamodb', 'GetItem', 200, RID + [('Content-Type', 'application/x-amz-json-1.0'), ('X-Dup', 'a'), ('x-dup', 'b')],
     b'{"Item": {"id": {"S": "1"}, "n": {"N": "1.50"}, "b": {"B": "aGk="}, "l": {"L": [{"NULL": true}]}}, "ConsumedCapacity": {"TableName": "t", "CapacityUnits": 0.5}}', 'iso'),
    ('dynamodb', 'DescribeTable', 200, RID,
     J({'Table': {'TableName': 't', 'CreationDateTime': 1700000000.123, 'ItemCount': 3, 'TableSizeBytes': 12345678901234567890,
                  'ProvisionedThroughput': {'LastIncreaseDateTime': 1700000000, 'ReadCapacityUnits': 5, 'NumberOfDecreasesToday': 0}}}), 'iso'),
    ('dynamodb', 'DescribeTable', 200, RID,
     J({'Table': {'TableName': 't', 'CreationDateTime': 1700000000.123, 'ProvisionedThroughput': {'LastIncreaseDateTime': 1700000000}}}), 'wire'),
    ('dynamodb', 'ListTables', 200, [], b'{"TableNames": ["a", "b"], "LastEvaluatedTableName": null}', 'iso'),
    ('dynamodb', 'GetItem', 400, RID,
     b'{"__type":"com.amazonaws.dynamodb.v20120810#ResourceNotFoundException","message":"Requested resource not found"}', 'iso'),
    ('dynamodb', 'GetItem', 400, RID,
     b'{"__type":"com.amazon.coral.validate#ValidationException","Message":"bad"}', 'iso'),
    ('dynamodb', 'GetItem', 500, RID, b'', 'iso'),
    ('dynamodb', 'GetItem', 503, [], b'<html><body>oops</body></html>', 'iso'),
    ('dynamodb', 'GetItem', 400, [], b'not json at all', 'iso'),
    ('dynamodb', 'GetItem', 413, [], b'', 'iso'),
    ('kms', 'Decrypt', 200, RID, b'{"KeyId": "k", "Plaintext": "c2VjcmV0", "EncryptionAlgorithm": "SYMMETRIC_DEFAULT"}', 'iso'),
    ('kms', 'Decrypt', 400, RID, b'{"__type": "NotFoundException", "message": "Key not found"}', 'iso'),
    ('kms', 'Decrypt', 400, RID, b'{"__type": "SomethingUnmodeled:http://internal", "message": "x"}', 'iso'),
    ('kms', 'DescribeKey', 200, RID,
     J({'KeyMetadata': {'KeyId': 'k', 'CreationDate': 1.7e9, 'Enabled': True, 'KeyUsage': 'ENCRYPT_DECRYPT', 'EncryptionAlgorithms': ['SYMMETRIC_DEFAULT']}}), 'iso'),
    ('logs', 'DescribeLogGroups', 200, RID,
     J({'logGroups': [{'logGroupName': 'g', 'creationTime': 1700000000000, 'retentionInDays': 7, 'storedBytes': 0}], 'nextToken': 'n'}), 'iso'),
    ('ssm', 'GetParameter', 200, RID,
     J({'Parameter': {'Name': '/a', 'Type': 'String', 'Value': 'v', 'Version': 3, 'LastModifiedDate': 1700000000.5, 'ARN': 'arn'}}), 'iso'),
    ('ssm', 'GetParameter', 400, RID, b'{"__type":"ParameterNotFound","message":""}', 'iso'),
    ('ecs', 'DescribeTasks', 200, RID,
     J({'tasks': [{'taskArn': 'a', 'createdAt': 1700000000.999999, 'cpu': '256', 'containers': [{'name': 'c', 'exitCode': 0}]}], 'failures': []}), 'iso'),
    ('sqs', 'GetQueueUrl', 400, RID + [('x-amzn-query-error', 'AWS.SimpleQueueService.NonExistentQueue;Sender')],
     b'{"__type":"com.amazonaws.sqs#QueueDoesNotExist","message":"The specified queue does not exist."}', 'iso'),
    ('sqs', 'GetQueueUrl', 400, RID + [('x-amzn-query-error', 'bad')], b'{"__type":"com.amazonaws.sqs#QueueDoesNotExist"}', 'iso'),
    ('sqs', 'ReceiveMessage', 200, RID,
     J({'Messages': [{'MessageId': 'm', 'Body': 'b', 'Attributes': {'SentTimestamp': '1'},
                      'MessageAttributes': {'a': {'DataType': 'Binary', 'BinaryValue': 'AAE='}}}]}), 'iso'),
    ('dynamodb', 'TagResource', 200, RID, b'', 'iso'),
    # rest-json
    ('lambda', 'GetFunction', 200, RID + [('Content-Type', 'application/json')],
     J({'Configuration': {'FunctionName': 'f', 'Timeout': 3, 'MemorySize': 128, 'LastModified': '2023-01-01T00:00:00.000+0000', 'CodeSize': 123},
        'Code': {'RepositoryType': 'S3', 'Location': 'https://x'}, 'Tags': {'a': 'b'}}), 'iso'),
    ('lambda', 'Invoke', 200, RID + [('X-Amz-Executed-Version', '$LATEST'), ('X-Amz-Log-Result', 'bG9n'), ('X-Amz-Function-Error', 'Unhandled')],
     b'{"errorMessage": "boom"}', 'iso'),
    ('lambda', 'Invoke', 202, RID, b'', 'iso'),
    ('lambda', 'GetFunction', 404, RID + [('x-amzn-ErrorType', 'ResourceNotFoundException:http://internal.amazon.com/coral/com.amazon.coral.service/')],
     b'{"Type":"User","Message":"Function not found: arn:aws:lambda:us-east-1:1:function:f"}', 'iso'),
    ('lambda', 'GetFunction', 429, RID, b'{"Reason":"ReservedFunctionConcurrentInvocationLimitExceeded","Type":"User","message":"Rate exceeded","retryAfterSeconds":"5","code":"TooManyRequestsException"}', 'iso'),
    ('lambda', 'GetFunction', 400, RID, b'{"code": 123, "message": "numeric code"}', 'iso'),
    ('lambda', 'GetFunction', 400, [('x-amz-request-id', 'r2'), ('x-amz-id-2', 'h2')], b'{"__type": "aws#InvalidParameterValueException", "message": "bad"}', 'iso'),
    ('lambda', 'ListTags', 200, [('x-amz-request-id', 'r2')], b'{"Tags": {}}', 'iso'),
    ('apigateway', 'GetRestApi', 200, RID,
     J({'id': 'a', 'name': 'n', 'createdDate': 1700000000, 'binaryMediaTypes': ['image/png'], 'minimumCompressionSize': 0, 'disableExecuteApiEndpoint': False}), 'iso'),
    ('apigateway', 'GetRestApi', 200, RID, J({'id': 'a', 'createdDate': 1700000000}), 'wire'),
    ('apigateway', 'GetExport', 200, RID + [('Content-Type', 'application/json'), ('Content-Disposition', 'attachment; filename="x.json"')],
     b'{"swagger": "2.0"}', 'iso'),
    ('efs', 'DescribeFileSystems', 200, RID,
     J({'FileSystems': [{'FileSystemId': 'fs-1', 'CreationTime': 1700000000.0, 'SizeInBytes': {'Value': 6144, 'Timestamp': 1700000001}, 'ProvisionedThroughputInMibps': 5, 'NumberOfMountTargets': 2.0, 'Encrypted': True}]}), 'iso'),
    ('iot', 'DescribeThing', 200, RID,
     J({'thingName': 't', 'attributes': {'k': 'v'}, 'version': 1, 'defaultClientId': 't'}), 'iso'),
    ('accessanalyzer', 'UpdateAnalyzer', 200, RID, J({'configuration': {'unusedAccess': {'unusedAccessAge': 90}}}), 'iso'),
    ('accessanalyzer', 'UpdateAnalyzer', 200, RID, J({'configuration': {'futureKind': {'x': 1}}}), 'iso'),
    ('b2bi', 'GetCapability', 200, RID,
     J({'capabilityId': 'c', 'configuration': {'__type': 'edi', 'edi': {'capabilityDirection': 'INBOUND', 'type': {'x12Details': {'version': 'VERSION_4010'}}}}}), 'iso'),
    ('b2bi', 'GetCapability', 200, RID, J({'capabilityId': 'c', 'configuration': {'other': None, 'brandNew': 'x'}}), 'iso'),
    ('opensearchserverless', 'GetIndex', 200, RID, b'{"indexSchema": {"mappings": {"a": [1, 2.0, null, {"b": "1e5"}]}, "n": 1E2}}', 'iso'),
    ('iot', 'DescribeThing', 404, RID + [('x-amzn-ErrorType', 'ResourceNotFoundException:')], b'{"message":"Thing t cannot be found."}', 'iso'),
    ('iot-data', 'GetThingShadow', 200, RID, b'{"state": {"desired": {"on": true}}}', 'iso'),
    ('mediastore-data', 'GetObject', 200, RID + [('Content-Length', '4'), ('Content-Type', 'text/plain'), ('ETag', '"e"'), ('Last-Modified', 'Tue, 29 Apr 2014 18:30:38 GMT'), ('Cache-Control', 'no-cache')],
     b'\x00\x01\xfe\xff', 'iso'),
    ('mediastore-data', 'GetObject', 200, RID + [('Last-Modified', 'Tue, 29 Apr 2014 18:30:38 GMT')], b'data', 'wire'),
    ('lex-runtime', 'PostContent', 200, RID + [('Content-Type', 'text/plain'), ('x-amz-lex-intent-name', 'Order'),
                                               ('x-amz-lex-slots', 'eyJzaXplIjogImxhcmdlIiwgIm4iOiAxLjB9'),
                                               ('x-amz-lex-session-attributes', 'e30='), ('x-amz-lex-dialog-state', 'ElicitSlot')],
     b'audio', 'iso'),
    ('dataexchange', 'SendApiAsset', 200, RID + [('X-Custom', 'v'), ('Content-Type', 'text/plain')], b'body text', 'iso'),
    ('datazone', 'GetLineageEvent', 200, RID + [('Event-Time', '2023-01-01T00:00:00Z'), ('Created-At', '1700000000'), ('Created-By', 'u')], b'{}', 'iso'),
    ('appconfig', 'GetHostedConfigurationVersion', 200, RID + [('Application-Id', 'a'), ('Version-Number', '7'), ('Content-Type', 'application/json')], b'{"x": 1}', 'iso'),
    ('appconfigdata', 'GetLatestConfiguration', 200, RID + [('Next-Poll-Configuration-Token', 't'), ('Next-Poll-Interval-In-Seconds', '60'), ('Version-Label', 'v1')], b'cfg', 'iso'),
    ('polly', 'SynthesizeSpeech', 200, RID + [('Content-Type', 'audio/mpeg'), ('x-amzn-RequestCharacters', '12')], b'mp3', 'iso'),
    ('grafana', 'DescribeWorkspaceConfiguration', 200, RID, J({'configuration': '{"plugins": {"pluginAdminEnabled": true}}', 'grafanaVersion': '9.4'}), 'iso'),
    ('efs', 'DeleteFileSystem', 204, RID, b'', 'iso'),
    # smithy-rpc-v2-cbor
    ('eventbridgev2', 'CreateEventBus', 200, RID + [('smithy-protocol', 'rpc-v2-cbor'), ('Content-Type', 'application/cbor')],
     cbor({'EventBusArn': 'arn', 'Name': 'bus', 'StorageConfiguration': {'RetentionPeriodInDays': 30, 'RetentionWindowStartTime': Tag1(1700000000)},
           'State': 'ACTIVE', 'CreationTime': Tag1(1700000000.5), 'Unknown': 1}), 'iso'),
    ('eventbridgev2', 'CreateEventBus', 200, RID,
     cbor({'EventBusArn': 'arn', 'CreationTime': Tag1(1700000000.5), 'StorageConfiguration': {'RetentionWindowStartTime': Tag1(1700000000)}}), 'wire'),
    ('eventbridgev2', 'PutEvents', 200, RID,
     cbor(Indef({'FailedEntryCount': 1, 'Entries': Indef([{'EventId': Indef(('ab', 'cd'))}, {'ErrorCode': 'E', 'ErrorMessage': '日'}, None])})), 'iso'),
    ('eventbridgev2', 'ListEventBuses', 200, RID,
     cbor({'EventBuses': [{'Name': 'a', 'CreationTime': Tag1(1700000000), 'LastModifiedTime': Tag1(1.7e9)}], 'NextToken': None}), 'iso'),
    ('eventbridgev2', 'DescribeEventBus', 400, RID,
     cbor({'__type': 'com.amazonaws.eventbridgev2#ResourceNotFoundException', 'message': 'nope'}), 'iso'),
    ('eventbridgev2', 'DescribeEventBus', 400, RID + [('x-amzn-query-error', 'AWS.Legacy;Sender')],
     cbor({'__type': 'aws.protocols#Unknown', 'Message': 'm'}), 'iso'),
    ('eventbridgev2', 'DescribeEventBus', 500, RID, b'', 'iso'),
    ('cloudwatchomni', 'GetContextGraph', 200, RID,
     cbor({'nodes': [], 'edges': [{'trafficStats': {'bytes': 2**40, 'packets': -5, 'flows': 0}}], 'other': Half(0x3C00)}), 'iso'),
    ('cloudwatchomni', 'GetAlert', 200, RID,
     cbor({'alert': {'name': 'n', 'rule': {'telemetryRule': {'condition': {'warningThreshold': Half(0x3E00), 'criticalThreshold': Single(0.1)}}}, 'createdAt': Tag1(1700000000)}}), 'iso'),
]


def make_response(svc, op, status, headers, body):
    hd = HTTPHeaderDict()
    for k, v in headers:
        hd.add(k, v)
    om = model(svc).operation_model(op)
    if status < 300 and om.has_streaming_output:
        body = StreamingBody(io.BytesIO(body), len(body))
    return {'headers': HeadersDict(hd), 'status_code': status, 'body': body, 'context': {'operation_name': op}}


def parse_case(svc, op, status, headers, body, fmt):
    m = model(svc)
    om = m.operation_model(op)
    factory = ResponseParserFactory()
    factory.set_parser_defaults(timestamp_parser=iso_format if fmt == 'iso' else identity, blob_parser=identity)
    parser = factory.create_parser(m.resolved_protocol)
    resp = make_response(svc, op, status, headers, body)
    parsed = parser.parse(resp, om.output_shape)
    if status >= 300:
        code = parsed.get('Error', {}).get('Code')
        if code is not None:
            shape = m.shape_for_error_code(code)
            if shape is not None:
                parsed.update(parser.parse(make_response(svc, op, status, headers, body), shape))
    if 'ResponseMetadata' in parsed:
        parsed['ResponseMetadata']['RetryAttempts'] = 0
    return {'service': svc, 'op': op, 'status': status, 'headers': headers, 'body': body, 'wire': fmt == 'wire', 'expected': dump(parsed)}


def main():
    w = sys.stdout.write
    w('//! Goldens generated by `generate.py` from the reference botocore.\n')
    w('//! Do not edit by hand.\n\n')
    w('use super::{ParseCase, SerCase};\n\n')
    w('pub const SER_CASES: &[SerCase] = &[\n')
    for args in SER:
        c = ser_case(*args)
        w('    SerCase {\n')
        w(f'        service: {rust_str(c["service"])},\n')
        w(f'        operation: {rust_str(c["op"])},\n')
        w(f'        params: {rust_str(json.dumps(c["params"]))},\n')
        w(f'        method: {rust_str(c["method"])},\n')
        w(f'        url_path: {rust_str(c["url_path"])},\n')
        w('        query: &[' + ', '.join(f'({rust_str(k)}, {rust_str(v)})' for k, v in c['query']) + '],\n')
        w('        headers: &[' + ', '.join(f'({rust_str(k)}, {rust_str(v)})' for k, v in c['headers']) + '],\n')
        w(f'        body: {rust_bytes(c["body"])},\n')
        hp = c['host_prefix']
        w(f'        host_prefix: {"None" if hp is None else "Some(" + rust_str(hp) + ")"},\n')
        w('    },\n')
    w('];\n\n')
    w('pub const PARSE_CASES: &[ParseCase] = &[\n')
    for args in PARSE:
        c = parse_case(*args)
        w('    ParseCase {\n')
        w(f'        service: {rust_str(c["service"])},\n')
        w(f'        operation: {rust_str(c["op"])},\n')
        w(f'        status: {c["status"]},\n')
        w('        headers: &[' + ', '.join(f'({rust_str(k)}, {rust_str(v)})' for k, v in c['headers']) + '],\n')
        w(f'        body: {rust_bytes(c["body"])},\n')
        w(f'        wire: {"true" if c["wire"] else "false"},\n')
        w(f'        expected: {rust_str(c["expected"])},\n')
        w('    },\n')
    w('];\n')


if __name__ == '__main__':
    main()
