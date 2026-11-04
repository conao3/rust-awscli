"""Print the command line of every help page of the reference AWS CLI, one
per line: `help`, `help topics`, `help <topic>`, and `<command path> help`
for every command of the command tables (services, operations, `wait`,
waiters, and the commands customizations add).

Usage: PYTHONPATH=<aws-cli checkout> python3 scripts/list-help-pages.py
"""

import awscrt.s3

if not hasattr(awscrt.s3, 'S3RetryConfig'):
    awscrt.s3.S3RetryConfig = type('S3RetryConfig', (), {})

from awscli.clidriver import create_clidriver
from awscli.topictags import TopicTagDB


def walk(path, cmd):
    print(' '.join(path), 'help')
    table = getattr(cmd, 'subcommand_table', None)
    for name, sub in (table or {}).items():
        if name != 'help':
            walk(path + [name], sub)


def main():
    print('help')
    print('help topics')
    db = TopicTagDB()
    db.load_json_index()
    for topic in db.get_all_topic_names():
        print('help', topic)
    for name in list(create_clidriver()._get_command_table()):
        walk([name], create_clidriver()._get_command_table()[name])


if __name__ == '__main__':
    main()
