"""Run the reference AWS CLI on one command line without sending requests.

Used by gen_cli_goldens.py. The parameters the CLI hands to the client
(with the CLI's own client-side handlers applied: binary format decoding
and the `before-parameter-build` customizations, and with botocore's
validation) are written to stderr as one `@@<json>` line; the client call
then returns an empty response.
"""

import json
import sys
import uuid

import awscli  # noqa: F401
import botocore
import botocore.client
from botocore.validate import validate_parameters

from awscli import clidriver
from awscli.customizations import waiters


def enc(v):
    if v is None or isinstance(v, (bool, str)):
        return v
    if isinstance(v, int):
        return {'__int__': str(v)}
    if isinstance(v, float):
        return {'__float__': repr(v)}
    if isinstance(v, (bytes, bytearray)):
        return {'__bytes__': bytes(v).hex()}
    if hasattr(v, 'read'):
        pos = v.tell()
        data = v.read()
        v.seek(pos)
        return {'__bytes__': data.hex()}
    if isinstance(v, (list, tuple)):
        return [enc(x) for x in v]
    if isinstance(v, dict):
        return {k: enc(x) for k, x in v.items()}
    return {'__repr__': repr(v)}


STATE = {}


def emit(rec):
    if 'done' not in STATE:
        STATE['done'] = True
        sys.stderr.write('@@' + json.dumps(rec) + '\n')


def strip_tokens(params, operation_model):
    shape = operation_model.input_shape
    if shape is None:
        return params
    out = dict(params)
    for name, member in shape.members.items():
        if member.metadata.get('idempotencyToken') and name in out:
            try:
                uuid.UUID(out[name])
                del out[name]
            except (TypeError, ValueError, AttributeError):
                pass
    return out


orig_make_client_call = clidriver.CLIOperationCaller._make_client_call


def make_client_call(self, client, operation_name, parameters, parsed_globals):
    py = botocore.xform_name(operation_name)
    paginate = bool(client.can_paginate(py) and parsed_globals.paginate)
    STATE['call'] = {'kind': 'call', 'operation': operation_name, 'paginate': paginate}
    if paginate:
        params = dict(parameters)
        pc = params.pop('PaginationConfig', {}) or {}
        STATE['call'].update(
            max_items=int(pc['MaxItems']) if 'MaxItems' in pc else None,
            page_size=int(pc['PageSize']) if 'PageSize' in pc else None,
            starting_token=pc.get('StartingToken'),
        )
        STATE['paginated_params'] = params
    else:
        STATE['call'].update(max_items=None, page_size=None, starting_token=None)
    return orig_make_client_call(self, client, operation_name, parameters, parsed_globals)


def wait_invoke(self, service_name, operation_name, parameters, parsed_globals):
    STATE['call'] = {'kind': 'wait', 'waiter': self._waiter_name}
    client = self._session.create_client(
        service_name,
        region_name=parsed_globals.region,
        endpoint_url=parsed_globals.endpoint_url,
        verify=parsed_globals.verify_ssl,
    )
    getattr(client, botocore.xform_name(operation_name))(**parameters)
    return 0


def fake_api_call(self, operation_name, api_params):
    operation_model = self._service_model.operation_model(operation_name)
    api_params = self._emit_api_params(
        api_params=api_params, operation_model=operation_model, context={}
    )
    if operation_model.input_shape is not None:
        validate_parameters(api_params, operation_model.input_shape)
    rec = dict(STATE.get('call') or {'kind': 'stub'})
    if 'paginated_params' in STATE:
        params = STATE['paginated_params']
    else:
        params = api_params
    rec['params'] = enc(strip_tokens(params, operation_model))
    emit(rec)
    if rec['kind'] == 'wait':
        raise SystemExit(0)
    return {}


clidriver.CLIOperationCaller._make_client_call = make_client_call
waiters.WaiterCaller.invoke = wait_invoke
botocore.client.BaseClient._make_api_call = fake_api_call
sys.exit(clidriver.main())
