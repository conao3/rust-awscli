# awsr

A fast Rust reimplementation of the [AWS CLI v2](https://github.com/aws/aws-cli) with the same command line and byte-for-byte identical output. Not an official AWS product.

`awsr` embeds the botocore service models of a pinned AWS CLI release (`models/VERSION`) and interprets them at run time, so every service and operation of that release is available as `awsr <service> <operation>`.

## Usage

```console
$ awsr sts get-caller-identity
$ awsr dynamodb list-tables --output table
$ awsr ec2 describe-instances --query 'Reservations[].Instances[].InstanceId' --output text
```

`awsr` reads the same `~/.aws/config`, `~/.aws/credentials`, SSO token cache, and environment variables as the AWS CLI.

## Performance

Mean wall time measured with `hyperfine -N` against a local stub endpoint (`--endpoint-url http://127.0.0.1:<port>`), static credentials, on an Intel Core Ultra 5 225U. The reference is the official AWS CLI 2.39.3 Linux installer.

| Command | awsr | aws 2.39.3 |
|---|---:|---:|
| `--version` | 4.7 ms | 502 ms |
| `sts get-caller-identity` | 9.6 ms | 614 ms |
| `dynamodb list-tables` | 18.9 ms | 706 ms |
| `ec2 describe-regions` | 33.7 ms | 819 ms |
| `ec2 help` | 136 ms | 958 ms |

The release binary is 30 MB, of which the embedded models, examples, and help inputs are 11 MB.

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
