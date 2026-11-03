"""Dump the reference CLI's command table as JSON.

For every service command backed by a botocore model, record its CLI name
and, for every operation command, the CLI operation name and the argument
table after all customizations (`awscli/clidriver.py`
`ServiceCommand._create_command_table` and
`ServiceOperation._create_argument_table`). Each argument is
`[name, serialized member or null, argument class, required]`.
"""

import json
import os
import sys

if os.environ.get("AWSR_REFERENCE_SRC"):
    sys.path.insert(0, os.environ["AWSR_REFERENCE_SRC"])

from awscli.clidriver import ServiceCommand, ServiceOperation, create_clidriver

GLOBAL_ARG_CLASSES = {
    "CliInputJSONArgument",
    "CliInputYAMLArgument",
    "GenerateCliSkeletonArgument",
}


def arg_entry(arg):
    return [
        arg.name,
        getattr(arg, "_serialized_name", None),
        type(arg).__name__,
        bool(getattr(arg, "required", False)),
    ]


def main():
    driver = create_clidriver()
    out = {}
    for cli_name, cmd in driver.subcommand_table.items():
        if not isinstance(cmd, ServiceCommand):
            continue
        operations = {}
        for op_cli, op in cmd._get_command_table().items():
            if not isinstance(op, ServiceOperation):
                continue
            name = op._operation_model.name
            if name in operations:
                continue
            try:
                table = list(op.arg_table.values())
            except Exception as e:  # noqa: BLE001
                print(f"{cli_name} {op_cli}: {e}", file=sys.stderr)
                table = []
            classes = {type(a).__name__ for a in table}
            operations[name] = {
                "cli": op_cli,
                "args": [
                    arg_entry(a)
                    for a in table
                    if type(a).__name__ not in GLOBAL_ARG_CLASSES
                ],
                "cli_input_json": "CliInputJSONArgument" in classes,
            }
        out[cmd._service_name] = {"cli": cli_name, "operations": operations}
    json.dump(out, sys.stdout, sort_keys=True, separators=(",", ":"))


if __name__ == "__main__":
    main()
