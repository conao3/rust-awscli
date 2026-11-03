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

## Development

```console
$ nix develop
$ make check
```

## License

Apache-2.0. The embedded models come from [aws/aws-cli](https://github.com/aws/aws-cli) (Apache-2.0).
