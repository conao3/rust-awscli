"""Generate src/cli/tables.rs from the reference AWS CLI.

Run from the devShell with HOME pointing at an empty directory (so no user
aliases or config leak in):

    HOME=$(mktemp -d) PYTHONPATH=$AWSR_REFERENCE_SRC \
        python3 src/cli/testdata/gen_tables.py > src/cli/tables.rs
"""

import sys

import awscli  # noqa: F401  (puts the vendored botocore on sys.path)
import botocore
from botocore import xform_name

from awscli.clidriver import ServiceCommand, ServiceOperation, create_clidriver
from awscli.customizations.waiters import WaitCommand


def rs(s):
    return '"' + s.replace('\\', '\\\\').replace('"', '\\"') + '"'


def main():
    driver = create_clidriver()
    table = driver._get_command_table()
    out = []
    w = out.append
    w('//! Data extracted from the reference AWS CLI by')
    w('//! `src/cli/testdata/gen_tables.py`. Do not edit.')
    w('')
    w('/// `botocore._xform_cache`: (name, sep, transformed).')
    w('pub(crate) static XFORM_CACHE: &[(&str, char, &str)] = &[')
    for (name, sep), v in botocore._xform_cache.items():
        if name in _seed_names:
            w(f"    ({rs(name)}, '{sep}', {rs(v)}),")
    w('];')
    w('')
    w('/// The main command table (`CLIDriver._get_command_table`): the CLI')
    w('/// name and the botocore service name, `None` for commands that are')
    w('/// not model-driven services.')
    w('pub(crate) static MAIN_COMMANDS: &[(&str, Option<&str>)] = &[')
    services = []
    for name, cmd in table.items():
        if type(cmd) is ServiceCommand:
            w(f'    ({rs(name)}, Some({rs(cmd._service_name)})),')
            services.append((name, cmd))
        else:
            w(f'    ({rs(name)}, None),')
    w('];')
    w('')
    w('/// Differences between each service command table and the one derived')
    w('/// from the model (`xform_name` of every operation, plus `wait` when')
    w('/// the service has waiters): (CLI service, removed names, renamed')
    w('/// entries (CLI name, operation), added commands that are not')
    w('/// model operations).')
    w('#[allow(clippy::type_complexity)]')
    w('pub(crate) static SERVICE_TABLE_DIFFS: &[(&str, &[&str], &[(&str, &str)], &[&str])] = &[')
    for name, cmd in services:
        model = cmd.service_model
        generic = [xform_name(op, '-') for op in model.operation_names]
        sub = cmd.subcommand_table
        generic_set = list(generic)
        try:
            wm = driver.session.get_waiter_model(model.service_name)
            if wm.waiter_names:
                generic_set.append('wait')
        except Exception:
            pass
        removed = [g for g in generic_set if g not in sub or not _is_generic(sub[g], g)]
        renamed = []
        custom = []
        for k, v in sub.items():
            if type(v) is ServiceOperation:
                if xform_name(v._operation_model.name, '-') != k:
                    renamed.append((k, v._operation_model.name))
            elif isinstance(v, WaitCommand) and k == 'wait':
                pass
            else:
                custom.append(k)
        if removed or renamed or custom:
            r = ', '.join(rs(x) for x in removed)
            n = ', '.join(f'({rs(a)}, {rs(b)})' for a, b in renamed)
            c = ', '.join(rs(x) for x in custom)
            w(f'    ({rs(name)}, &[{r}], &[{n}], &[{c}]),')
    w('];')
    print('\n'.join(out))


def _is_generic(obj, key):
    if key == 'wait':
        return isinstance(obj, WaitCommand)
    return (
        type(obj) is ServiceOperation
        and xform_name(obj._operation_model.name, '-') == key
    )


_seed_names = {name for (name, _sep) in dict(botocore._xform_cache)}

if __name__ == '__main__':
    main()
