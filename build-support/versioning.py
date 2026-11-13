"""Display labels and numeric builds: a build encodes the version it packages (identity.encode_build)."""
import re
import os
import subprocess
from identity import IDENTITY, decode_build, encode_build
from provenance import ROOT, product_source


def validate_source_commit(source_commit):
    if not re.fullmatch(r'[0-9a-f]{40}', source_commit):
        raise ValueError('source commit must be the full lowercase 40-character Git commit')


def inhouse_version():
    inhouse = IDENTITY['inhouse_version']
    if not isinstance(inhouse, str) or not re.fullmatch(r'(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)', inhouse):
        raise ValueError('inhouse_version must be a numeric major.minor.patch version')
    return inhouse


def package_build(sequence, inhouse, channel=None):
    """In-house packages number inhouse_version's builds; public ones the manifest version's."""
    channel = channel or os.environ.get('OINK_PACKAGE_CHANNEL')
    return encode_build(inhouse_version() if inhouse or channel == 'beta' else IDENTITY['version'].split('-')[0], sequence)


def encoded_sequence(version, build):
    """The sequence n of `build`; rejects a build that does not encode `version`."""
    encoded, sequence = decode_build(build)
    if encoded != version:
        raise ValueError(f'build {build} encodes {encoded}, but this package is {version}')
    return sequence


def inhouse_label(build):
    """`<inhouse_version>-inhouse.<n>`: the display label of an in-house build (PC and phone)."""
    version = inhouse_version()
    return f'{version}-inhouse.{encoded_sequence(version, build)}'


def beta_label(build):
    version = inhouse_version()
    return f'{version}-beta.{encoded_sequence(version, build)}'


def release_version(base, source_commit, build, channel=None):
    """Display label of a package; rejects a build that does not encode the labelled version."""
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:-beta\.[0-9]+)?', base):
        raise ValueError('packages require a stable or beta.N base version')
    channel = channel or os.environ.get('OINK_PACKAGE_CHANNEL')
    if channel == 'beta':
        if source_commit is not None:
            raise ValueError('beta packages cannot use an in-house source label')
        return beta_label(build)
    if channel not in (None, '', 'inhouse'):
        raise ValueError('unknown package channel')
    if source_commit is not None:
        validate_source_commit(source_commit)
        return inhouse_label(build)
    encoded_sequence(base.split('-')[0], build)
    return base


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
    if decode_build(build)[0] != version.split('-')[0]:
        raise ValueError('CFBundleVersion must encode CFBundleShortVersionString')
    return dict(CFBundleShortVersionString=version.split('-')[0], CFBundleVersion=str(build),
                CFBundleGetInfoString=version)
