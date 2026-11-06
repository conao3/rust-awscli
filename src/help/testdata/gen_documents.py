"""Generate the reference's help documents (`doc.getvalue()`, the
reStructuredText handed to the renderer) that `help::tests` compares with.

    HOME=$(mktemp -d) PYTHONPATH=$AWSR_REFERENCE_SRC \
        python3 src/help/testdata/gen_documents.py src/help/testdata
"""

import os
import sys

import awscli.help
from awscli.clidriver import create_clidriver

PAGES = {
    'help-topics.rst': ['help', 'topics'],
    'proton-wait.rst': ['proton', 'wait', 'help'],
    'sts-get-caller-identity.rst': ['sts', 'get-caller-identity', 'help'],
    'configure-list.rst': ['configure', 'list', 'help'],
}


class Capture:
    def __init__(self):
        self.contents = None

    def render(self, contents):
        self.contents = contents


def main():
    out = sys.argv[1]
    for name, argv in PAGES.items():
        capture = Capture()
        awscli.help.get_renderer = lambda help_output: capture
        create_clidriver().main(argv)
        with open(os.path.join(out, name), 'wb') as f:
            f.write(capture.contents)


main()
