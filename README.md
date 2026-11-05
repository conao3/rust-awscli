# awsr

A fast Rust reimplementation of the [AWS CLI v2](https://github.com/aws/aws-cli) with the same command line and byte-for-byte identical output. Not an official AWS product.

`awsr` embeds the botocore service models of a pinned AWS CLI release (`models/VERSION`) and interprets them at run time, so every service and operation of that release is available as `awsr <service> <operation>`.

## Install

Prebuilt binaries for Linux (x86_64, x86_64 musl, aarch64) and macOS (x86_64, aarch64) are attached to the [GitHub releases](https://github.com/conao3/rust-awscli/releases).

```console
$ nix profile install github:conao3/rust-awscli
$ cargo install --git https://github.com/conao3/rust-awscli awsr
```

## Usage

```console
$ awsr sts get-caller-identity
$ awsr dynamodb list-tables --output table
$ awsr ec2 describe-instances --query 'Reservations[].Instances[].InstanceId' --output text
```

`awsr` reads the same `~/.aws/config`, `~/.aws/credentials`, SSO token cache, and environment variables as the AWS CLI.

## Performance

Mean wall time of the flake package (statically linked) measured with `hyperfine -N` against a local stub endpoint (`--endpoint-url http://127.0.0.1:<port>`), static credentials, on an Intel Core Ultra 5 225U. The reference is the official AWS CLI 2.39.3 Linux installer. A C hello world takes 1.1 ms on the same machine.

| Command | awsr | aws 2.39.3 |
|---|---:|---:|
| `--version` | 1.6 ms | 548 ms |
| `sts get-caller-identity` | 4.7 ms | 704 ms |
| `dynamodb list-tables` | 5.1 ms | 687 ms |
| `ec2 describe-regions` | 5.4 ms | 981 ms |
| `help` | 1.8 ms (57 ms the first time) | 704 ms |
| `ec2 help` | 1.9 ms (107 ms the first time) | 1008 ms |

The service models and the help inputs are embedded uncompressed in a form that is read in place, so a command touches only the parts it uses and parses no JSON at start. This makes the binary 121 MB. A help page is formatted by the installed groff, like the reference does, the first time it is shown; the output is then kept in `$XDG_CACHE_HOME/awsr/help` (or `~/.cache/awsr/help`), keyed by the page, the awsr and groff binaries, and the groff and locale environment variables.

## Development

```console
$ nix develop
$ make check
```

### Differential testing

`awsr-difftest` (`difftest/`) runs `awsr` and the reference CLI on the same command line against a stub endpoint that answers with responses generated from the operation's output shape, and compares stdout, stderr, the exit code, the streaming output file, and the recorded requests.

```console
$ make difftest        # the cases in difftest/smoke.txt
$ make difftest-full   # every operation of every service, 4 seeds, json / text / table
```

Reference runs are stored in `target/difftest/refcache`, keyed by the reference build and the prepared case, and replayed on later runs. Runs that are not stored are forked from one reference process that has loaded the CLI. `--refresh-ref-cache` replaces the stored runs, `--no-ref-cache` runs the reference for every case, and `--no-refserver` starts a new reference process for every run. Operations that cannot be compared against a stub are listed with their reasons in `difftest/exclusions.txt`.

## License

Apache-2.0. The embedded models come from [aws/aws-cli](https://github.com/aws/aws-cli) (Apache-2.0).
