# AGENTS.md

`awsr` is a Rust reimplementation of the AWS CLI v2 whose command line, requests, and output (json / text / table / yaml / yaml-stream / off, errors, exit codes) match the reference AWS CLI byte for byte. Commands are driven by the botocore models embedded from the reference version (`models/VERSION`).

## Reference

- Reference version: the AWS CLI release in `models/VERSION`. `scripts/vendor-models.sh <aws-cli checkout>` regenerates `models/` from the `awscli/botocore/data/` and `awscli/data/cli.json` of that release, the example and topic documents (`awscli/examples/`, `awscli/topics/`), and the help inputs the customizations build at run time (`scripts/gen-help-data.py`).
- The devShell provides the reference CLI (official Linux installer) as `aws` and in `$AWSR_REFERENCE_AWS`, and a Python with the reference's dependencies. `$AWSR_REFERENCE_SRC` points at a checkout of the reference tag; `PYTHONPATH=$AWSR_REFERENCE_SRC python3` imports `awscli` and `awscli.botocore` from source, which is how expected values for unit tests are produced.
- Behavior is ported from the reference source, not reinvented. When a rule is ported, the doc comment names the Python file and function it comes from.

## Layout

- `src/value.rs`: the dynamic value (Python `dict` / `list` / `str` / `int` / `float` / `bool` / `None` / `bytes`).
- `src/model.rs`: the embedded models (`models/services/<service>.json.zst`).
- `src/cli/`: global options, command tables, argument parsing, parameter values, validation.
- `src/config.rs`, `src/session.rs`: configuration, credentials, signing, HTTP, retries.
- `src/endpoint/`: the endpoint rule set interpreter.
- `src/protocol/`: serializers and parsers for json, rest-json, smithy-rpc-v2-cbor, query, ec2, rest-xml.
- `src/handlers.rs`: botocore handlers keyed by model traits and the service-specific handlers.
- `src/paginate.rs`, `src/waiter.rs`, `src/jmes.rs`: pagination, waiters, JMESPath.
- `src/output/`: formatters and error rendering.
- `src/help/`: the help pages: the document builders (`clidocs.py`, `bcdoc`), the docutils rst parser and manpage writer (`docutils/`), and groff / mandoc and the pager. `scripts/compare-help.sh` compares them with the reference.
- `src/driver.rs`: one command line end to end.

## Conventions

- Commit messages, comments, and docs are in English.
- Keep Python's semantics where output depends on them (number types, `str()`, `repr(float)`, sort order, `isoformat()`).

## Verification

```console
$ nix develop --command make check
$ nix build
```
