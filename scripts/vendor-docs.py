"""Write the reference's example and topic documents as JSON maps of
relative path to contents (read with Python's universal newlines, as the
reference reads them).

Usage: python3 scripts/vendor-docs.py <aws-cli checkout> OUTDIR

Writes OUTDIR/examples/<dir>.json for each directory of awscli/examples
(OUTDIR/examples/_root.json for the files at its top) and
OUTDIR/topics.json for awscli/topics.
"""

import json
import os
import sys


def read_tree(root):
    files = {}
    for dirpath, _, names in os.walk(root):
        for name in names:
            path = os.path.join(dirpath, name)
            with open(path, encoding='utf-8') as f:
                files[os.path.relpath(path, root)] = f.read()
    return dict(sorted(files.items()))


def write(path, data):
    with open(path, 'w', encoding='utf-8') as f:
        json.dump(data, f, ensure_ascii=False, separators=(',', ':'))


def main():
    src, out = sys.argv[1], sys.argv[2]
    examples = os.path.join(src, 'awscli', 'examples')
    os.makedirs(os.path.join(out, 'examples'), exist_ok=True)
    root = {}
    for entry in sorted(os.listdir(examples)):
        path = os.path.join(examples, entry)
        if os.path.isdir(path):
            write(os.path.join(out, 'examples', f'{entry}.json'), read_tree(path))
        else:
            with open(path, encoding='utf-8') as f:
                root[entry] = f.read()
    write(os.path.join(out, 'examples', '_root.json'), root)
    write(os.path.join(out, 'topics.json'), read_tree(os.path.join(src, 'awscli', 'topics')))


if __name__ == '__main__':
    main()
