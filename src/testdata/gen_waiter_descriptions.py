"""Generate src/testdata/waiter-descriptions.json:
`WaiterStateDocBuilder.build_waiter_state_description` over every waiter of
the reference models, in the order `waiter::tests` walks them (services
sorted by `<name>/`, waiters by name), as a count, a SHA-256 of the
descriptions joined by newlines, and three samples.

    PYTHONPATH=$AWSR_REFERENCE_SRC python3 src/testdata/gen_waiter_descriptions.py \
        > src/testdata/waiter-descriptions.json
"""

import hashlib
import json

import awscli  # noqa: F401  (puts the vendored botocore on sys.path)
import botocore.session
from botocore.exceptions import DataNotFoundError

from awscli.customizations.waiters import WaiterStateDocBuilder


def description(model, name):
    return WaiterStateDocBuilder(
        model.get_waiter(name)
    ).build_waiter_state_description()


def main():
    session = botocore.session.get_session()
    names = session.get_available_services()
    descs = []
    models = {}
    for name in sorted(names, key=lambda n: f'{n}/'):
        try:
            model = session.get_waiter_model(name)
        except DataNotFoundError:
            continue
        models[name] = model
        for waiter in sorted(model.waiter_names):
            descs.append(description(model, waiter))
    print(json.dumps({
        'desc_count': len(descs),
        'desc_sha256': hashlib.sha256('\n'.join(descs).encode()).hexdigest(),
        'desc_sample': [
            description(models['ec2'], 'InstanceRunning'),
            description(models['s3'], 'BucketExists'),
            description(models['dynamodb'], 'TableExists'),
        ],
    }, indent=1))


main()
