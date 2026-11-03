"""Extract the help inputs the reference builds at runtime from its
customizations: the argument tables of every operation and waiter command,
the commands customizations add (`BasicCommand` DESCRIPTION / SYNOPSIS /
EXAMPLES / ARG_TABLE / SUBCOMMANDS), and the undocumented commands.

Usage: PYTHONPATH=<aws-cli checkout> python3 scripts/gen-help-data.py OUTDIR

Writes OUTDIR/<cli service>.json ({"undocumented", "operations",
"no_output" (operations whose help documents no output), "waiters",
"docs", "models"}) for every model-driven service and
OUTDIR/_custom.json ({"undocumented", "commands", "docs", "models"}) for the
main command table and the `BasicCommand`s keyed by command path.

Argument records are dicts with short keys, only non-default ones:
  n   argument table key            c   cli_name when not `--<n>`
  t   cli_type_name                 r   required
  ar  _DOCUMENT_AS_REQUIRED         g   group_name
  p   positional_arg                na  nargs
  sy  synopsis                      u   _UNDOCUMENTED
  d   documentation (index into `docs`; absent: the model member's)
  m   input member name (the argument model is the operation's input member)
  s   argument model (index into `models`: {"top": name, "member": traits,
      "shapes": map}, with "svc": true instead of "shapes" when the shapes
      are the service model's, and "members" when a customization removed
      members of the top shape)
"""

import json
import os
import sys

import awscrt.s3

if not hasattr(awscrt.s3, 'S3RetryConfig'):
    awscrt.s3.S3RetryConfig = type('S3RetryConfig', (), {})

from awscli.clidriver import ServiceCommand, ServiceOperation, create_clidriver
from awscli.customizations.commands import BasicCommand
from awscli.customizations.waiters import WaitCommand, WaiterStateCommand


class Interner:
    def __init__(self):
        self.items = []
        self._index = {}

    def __call__(self, value):
        key = json.dumps(value, sort_keys=True)
        if key not in self._index:
            self._index[key] = len(self.items)
            self.items.append(value)
        return self._index[key]


class Output:
    def __init__(self):
        self.docs = Interner()
        self.models = Interner()

    def shape_ref(self, shape, service_model):
        resolver = shape._shape_resolver
        if not hasattr(resolver, '_shape_map'):
            ref = {'top': shape.name, 'shapes': {shape.name: shape._shape_model}}
            return self.models(ref)
        base = resolver._shape_map[shape.name]
        member = {
            k: v
            for k, v in shape._shape_model.items()
            if k not in base or base[k] != v
        }
        ref = {'top': shape.name}
        if member:
            ref['member'] = member
        if shape.type_name == 'structure' and list(shape.members) != list(
            shape._shape_model.get('members', {})
        ):
            ref['members'] = list(shape.members)
        if (
            service_model is not None
            and resolver._shape_map == service_model._shape_resolver._shape_map
        ):
            ref['svc'] = True
        else:
            ref['shapes'] = resolver._shape_map
        return self.models(ref)

    def arg_record(self, name, arg, operation_model):
        rec = {'n': name}
        if arg.cli_name != '--' + name:
            rec['c'] = arg.cli_name
        model = getattr(arg, 'argument_model', None)
        service_model = operation_model.service_model if operation_model else None
        input_shape = operation_model.input_shape if operation_model else None
        serialized = getattr(arg, '_serialized_name', None)
        is_member = (
            model is not None
            and input_shape is not None
            and serialized is not None
            and serialized in input_shape.members
            and model is input_shape.members[serialized]
        )
        if is_member:
            rec['m'] = serialized
        elif model is not None:
            rec['s'] = self.shape_ref(model, service_model)
        if not (is_member and arg.documentation == model.documentation):
            rec['d'] = self.docs(arg.documentation)
        if not (model is not None and arg.cli_type_name == model.type_name):
            rec['t'] = arg.cli_type_name
        if arg.required:
            rec['r'] = True
        if getattr(arg, '_DOCUMENT_AS_REQUIRED', False):
            rec['ar'] = True
        if arg.group_name is not None:
            rec['g'] = arg.group_name
        if getattr(arg, 'positional_arg', False):
            rec['p'] = True
        nargs = getattr(arg, 'nargs', None)
        if nargs is not None:
            rec['na'] = nargs
        synopsis = getattr(arg, 'synopsis', None)
        if synopsis:
            rec['sy'] = synopsis
        if getattr(arg, '_UNDOCUMENTED', False):
            rec['u'] = True
        return rec

    def arg_records(self, arg_table, operation_model):
        return [
            self.arg_record(name, arg, operation_model)
            for name, arg in arg_table.items()
        ]

    def tables(self):
        return {'docs': self.docs.items, 'models': self.models.items}


def undocumented(table):
    return [n for n, c in table.items() if hasattr(c, '_UNDOCUMENTED')]


def write(path, data):
    with open(path, 'w') as f:
        json.dump(data, f, separators=(',', ':'))


def main():
    out_dir = sys.argv[1]
    os.makedirs(out_dir, exist_ok=True)
    driver = create_clidriver()
    custom = Output()
    commands = {}

    def walk_basic(path, cmd):
        subcommands = cmd.subcommand_table
        help_command = cmd.create_help_command()
        commands[' '.join(path)] = {
            'name': help_command.name,
            'description': help_command.description,
            'synopsis': help_command.synopsis,
            'examples': help_command.examples,
            'commands': list(help_command.command_table),
            'undocumented': undocumented(help_command.command_table),
            'args': custom.arg_records(help_command.arg_table, None),
        }
        for name, sub in subcommands.items():
            if not isinstance(sub, BasicCommand):
                raise TypeError(f'{path} {name}: {type(sub)}')
            walk_basic(path + [name], sub)

    for name in list(driver._get_command_table()):
        # A fresh driver per command: the plugins a command lazily
        # initializes stay registered for the rest of the process.
        cmd = create_clidriver()._get_command_table()[name]
        if not isinstance(cmd, ServiceCommand):
            walk_basic([name], cmd._resolve() if hasattr(cmd, '_resolve') else cmd)
            continue
        service = Output()
        table = cmd._get_command_table()
        operations = {}
        no_output = []
        waiters = {}
        for op_name, op in table.items():
            if isinstance(op, WaitCommand):
                for w_name, w in op.subcommand_table.items():
                    if not isinstance(w, WaiterStateCommand):
                        raise TypeError(f'{name} wait {w_name}: {type(w)}')
                    waiters[w_name] = service.arg_records(
                        w.arg_table, w._operation_model
                    )
            elif isinstance(op, ServiceOperation):
                operations[op_name] = service.arg_records(
                    op.arg_table, op._operation_model
                )
                if (
                    type(op).create_help_command
                    is not ServiceOperation.create_help_command
                    and op.create_help_command().obj.output_shape is None
                ):
                    no_output.append(op_name)
            elif isinstance(op, BasicCommand):
                walk_basic([name, op_name], op)
            else:
                raise TypeError(f'{name} {op_name}: {type(op)}')
        write(
            os.path.join(out_dir, f'{name}.json'),
            {
                'undocumented': undocumented(table),
                'operations': operations,
                'no_output': no_output,
                'waiters': waiters,
                **service.tables(),
            },
        )
    write(
        os.path.join(out_dir, '_custom.json'),
        {
            'undocumented': undocumented(driver._get_command_table()),
            'commands': commands,
            **custom.tables(),
        },
    )


if __name__ == '__main__':
    main()
