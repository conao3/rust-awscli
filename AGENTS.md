# AGENTS.md

`awsr` is a Rust reimplementation of the AWS CLI v2 whose command line, requests, and output (json / text / table / yaml / yaml-stream / off, errors, exit codes) match the reference AWS CLI byte for byte, except where it names itself (`src/progname.rs`): `--version`, the usage lines, the `awsr:` prefix of messages, and the command lines of the help pages. Commands are driven by the botocore models embedded from the reference version (`models/VERSION`).

## Reference

- Reference version: the AWS CLI release in `models/VERSION`. `scripts/vendor-models.sh <aws-cli checkout>` regenerates `models/` from the `awscli/botocore/data/` and `awscli/data/cli.json` of that release, the example and topic documents (`awscli/examples/`, `awscli/topics/`), and the help inputs the customizations build at run time (`scripts/gen-help-data.py`).
- `scripts/update-reference.sh <version>` moves the reference: it pins the source and the Linux installer of that release in `flake.nix`, runs `scripts/vendor-models.sh`, and regenerates the goldens derived from the models and help documents (`src/cli/tables.rs`, `src/cli/fingerprints.rs`, `src/help/testdata/*.rst`, `src/testdata/waiter-descriptions.json`). The `Reference` workflow runs it daily for the latest AWS CLI release, runs `make check`, the difftest corpus of the services whose models changed, and `scripts/compare-help.sh` over their help pages, and opens a PR `reference/<version>` with the results (a draft when any of them fails).
- The devShell provides the reference CLI (official Linux installer) as `aws` and in `$AWSR_REFERENCE_AWS`, and a Python with the reference's dependencies. `$AWSR_REFERENCE_SRC` points at a checkout of the reference tag; `PYTHONPATH=$AWSR_REFERENCE_SRC python3` imports `awscli` and `awscli.botocore` from source, which is how expected values for unit tests are produced. The golden generators import `scripts/awsr_progname.py` first, which loads the reference with the name `awsr` in its usage lines and message prefixes.
- Behavior is ported from the reference source, not reinvented. When a rule is ported, the doc comment names the Python file and function it comes from.

## Layout

- `src/value.rs`: the dynamic value (Python `dict` / `list` / `str` / `int` / `float` / `bool` / `None` / `bytes`).
- `models/`: the vendored data, split into crates of less than the 10 MB crates.io limit that hold only files and their directory (`DIR`): `awsr-models` (`models/`: CLI data, endpoints, examples, topics, help inputs, `VERSION`), `awsr-models-services-a-l` and `awsr-models-services-m-z` (`models/services-a-l/`, `models/services-m-z/`: the service models by the first letter of the name). Their version is the reference version. `awsr` takes them as build dependencies: `build.rs` reads the files under each `DIR` and embeds them, and the data crates add no code to the binary.
- `src/model.rs`: the embedded models (`models/services-*/<service>.json.zst`), which `build.rs` encodes at build time into `src/bjson.rs` documents that are read in place.
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
