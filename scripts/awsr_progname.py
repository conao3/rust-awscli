import importlib.abc
import importlib.machinery
import re
import sys

RULES = [
    (re.compile(r'aws: \['), 'awsr: ['),
    (re.compile(r'usage: aws '), 'usage: awsr '),
    (re.compile(r'"  aws '), '"  awsr '),
    (re.compile(r'"aws \[options\]'), '"awsr [options]'),
    (re.compile(r'prog="aws"'), 'prog="awsr"'),
]


class Loader(importlib.machinery.SourceFileLoader):
    def get_data(self, path):
        data = super().get_data(path)
        if not path.endswith('.py'):
            return data
        text = data.decode()
        for pattern, repl in RULES:
            text = pattern.sub(repl, text)
        return text.encode()

    def get_code(self, fullname):
        return self.source_to_code(self.get_data(self.path), self.path)


class Finder(importlib.abc.MetaPathFinder):
    def find_spec(self, name, path, target=None):
        if name != 'awscli' and not name.startswith('awscli.'):
            return None
        spec = importlib.machinery.PathFinder.find_spec(name, path, target)
        if spec is not None and isinstance(spec.loader, importlib.machinery.SourceFileLoader):
            spec.loader = Loader(spec.loader.name, spec.loader.path)
        return spec


sys.meta_path.insert(0, Finder())
