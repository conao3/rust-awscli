"""Generate src/cli/tables.rs from the reference AWS CLI.

Run from the devShell with HOME pointing at an empty directory (so no user
aliases or config leak in):

    HOME=$(mktemp -d) PYTHONPATH=$AWSR_REFERENCE_SRC \
        python3 src/cli/testdata/gen_tables.py > src/cli/tables.rs
"""

import sys

import awscrt.s3

if not hasattr(awscrt.s3, 'S3RetryConfig'):
    awscrt.s3.S3RetryConfig = type('S3RetryConfig', (), {})

import awscli  # noqa: F401  (puts the vendored botocore on sys.path)
import botocore
from botocore import xform_name

from awscli.argparser import ArgTableArgParser
from awscli.arguments import BooleanArgument
from awscli.clidriver import ServiceCommand, ServiceOperation, create_clidriver
from awscli.customizations.commands import BasicCommand
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
    for name, _ in services:
        cmd = create_clidriver()._get_command_table()[name]
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
    w('')
    w('/// The options of the customization commands (`BasicCommand`) that')
    w('/// take no value (their argparse action has `nargs=0`): (command')
    w('/// path, option names).')
    w('pub(crate) static CUSTOM_FLAGS: &[(&str, &[&str])] = &[')
    for path, flags in custom_flags():
        w(f"    ({rs(' '.join(path))}, &[{', '.join(rs(f) for f in flags)}]),")
    w('];')
    w('')
    w('/// The options of the operation commands that take no value besides')
    w('/// the `--<name>` / `--no-<name>` pairs of `BooleanArgument`: (CLI')
    w('/// service and operation, option names).')
    w('pub(crate) static OPERATION_FLAGS: &[(&str, &[&str])] = &[')
    for path, flags in operation_flags():
        w(f"    ({rs(' '.join(path))}, &[{', '.join(rs(f) for f in flags)}]),")
    w('];')
    print('\n'.join(out))


def operation_flags():
    """The option names of each operation whose action takes no value and
    that are not those of a `BooleanArgument`."""
    found = []
    for name in list(create_clidriver()._get_command_table()):
        cmd = create_clidriver()._get_command_table()[name]
        if not isinstance(cmd, ServiceCommand):
            continue
        for op_name, op in cmd._get_command_table().items():
            if not isinstance(op, ServiceOperation):
                continue
            parser = ArgTableArgParser(op.arg_table)
            boolean = set()
            for key, arg in op.arg_table.items():
                if isinstance(arg, BooleanArgument):
                    boolean.update([key, 'no-' + key])
            flags = [
                option[2:]
                for action in parser._actions
                if action.nargs == 0
                for option in action.option_strings
                if option[2:] not in boolean
            ]
            if flags:
                found.append(([name, op_name], flags))
    return found


def custom_flags():
    """The option names of each `BasicCommand` whose action takes no value."""
    found = []

    def walk(path, cmd):
        subcommands = cmd.subcommand_table
        parser = ArgTableArgParser(cmd.arg_table)
        flags = [
            action.option_strings[0][2:]
            for action in parser._actions
            if action.option_strings and action.nargs == 0
        ]
        if flags:
            found.append((path, flags))
        for name, sub in subcommands.items():
            if isinstance(sub, BasicCommand):
                walk(path + [name], sub)

    for name in list(create_clidriver()._get_command_table()):
        cmd = create_clidriver()._get_command_table()[name]
        if hasattr(cmd, '_resolve'):
            cmd = cmd._resolve()
        if isinstance(cmd, ServiceCommand):
            for op_name, op in cmd._get_command_table().items():
                if isinstance(op, BasicCommand) and not isinstance(op, WaitCommand):
                    walk([name, op_name], op)
        elif isinstance(cmd, BasicCommand):
            walk([name], cmd)
    return found


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
