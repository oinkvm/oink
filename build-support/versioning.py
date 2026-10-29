"""In-house display labels and ordering use release builds; commits record provenance."""
import re
import subprocess
from identity import IDENTITY
from provenance import ROOT, product_source


def validate_source_commit(source_commit):
    if not re.fullmatch(r'[0-9a-f]{40}', source_commit):
        raise ValueError('source commit must be the full lowercase 40-character Git commit')


def release_version(base, source_commit=None, build=None):
    if source_commit is None:
        return base
    validate_source_commit(source_commit)
    if type(build) is not int or not 1 <= build < 2**31:
        raise ValueError('in-house build must be an integer in 1..2147483647')
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:-beta\.[0-9]+)?', base):
        raise ValueError('in-house versions require a stable or beta.N base version')
    inhouse = IDENTITY['inhouse_version']
    if not isinstance(inhouse, str) or not re.fullmatch(r'(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)', inhouse):
        raise ValueError('inhouse_version must be a numeric major.minor.patch version')
    return f'{inhouse}-inhouse.{build}'


def local_source_commit(explicit=None, root=ROOT):
    """Archives require the originating commit as an explicit build input."""
    if explicit is not None:
        validate_source_commit(explicit)
    if not (root / '.git').exists():
        if explicit is None:
            raise ValueError('source archives require --source-commit from the originating checkout')
        return explicit
    head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
    if explicit is not None and explicit != head:
        raise ValueError('source commit differs from checkout HEAD')
    changed = subprocess.check_output(['git', 'diff', '--name-only', '-z', 'HEAD'], cwd=root)
    untracked = subprocess.check_output(['git', 'ls-files', '--others', '--exclude-standard', '-z'], cwd=root)
    if any(product_source(path.decode()) for path in (changed + untracked).split(b'\0') if path):
        raise ValueError('in-house packages require committed product sources')
    return head


def macos_version_fields(version, build):
    return dict(CFBundleShortVersionString=version.split('-')[0], CFBundleVersion=str(build),
                CFBundleGetInfoString=version)
