#!/usr/bin/env python3
"""iOS package checks.

    ios_package.py shortcut [--months N]   the bundled signed shortcuts: each matches its source, and
                                           its signing certificate is valid for N more months (default 3)
    ios_package.py manifest                record the signed shortcuts (sign-shortcut.sh runs it)
    ios_package.py extensions <Oink.app>   every app extension has the app's version and build

The shortcuts (apps/mobile/ios/Shortcuts/OinkCopy-<colour>.shortcut, one per tile colour,
each signed from OinkCopy-<colour>.unsigned.plist) are signed with
`shortcuts sign --mode anyone`: an Apple Encrypted Archive (AEA1) whose auth data is a
property list with the signing certificate chain. The leaf certificate lasts about 13
months; re-sign them with sign-shortcut.sh (macOS, iCloud signed in) before the check fails.
packaging/tests/test_ios_shortcut.py runs the shortcut check in CI.
"""
import argparse
import datetime
import hashlib
import json
import plistlib
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SHORTCUTS = ROOT / 'apps/mobile/ios/Shortcuts'
MANIFEST_NAME = 'OinkCopy.signed.json'


class CheckError(Exception):
    pass


def signing_chain(data):
    """DER certificates of a signed shortcut, leaf first."""
    if data[:4] != b'AEA1' or len(data) < 12:
        raise CheckError('not an AEA1 signed shortcut')
    length = struct.unpack('<I', data[8:12])[0]
    try:
        context = plistlib.loads(data[12:12 + length])
    except Exception as error:
        raise CheckError(f'unreadable signing context: {error}') from error
    chain = context.get('SigningCertificateChain') if isinstance(context, dict) else None
    if not chain or not all(isinstance(item, bytes) for item in chain):
        raise CheckError('no signing certificate chain (only `--mode anyone` signatures are bundled)')
    return chain


def _tlv(data, offset):
    """(tag, value start, value end) of the DER element at offset."""
    tag, first = data[offset], data[offset + 1]
    offset += 2
    if first < 0x80:
        length = first
    else:
        count = first & 0x7F
        length = int.from_bytes(data[offset:offset + count], 'big')
        offset += count
    return tag, offset, offset + length


def not_after(der):
    """The certificate's validity end (X.509 tbsCertificate.validity.notAfter), UTC."""
    _, start, _ = _tlv(der, 0)                      # Certificate
    tag, start, _ = _tlv(der, start)                # tbsCertificate
    tag, value, end = _tlv(der, start)
    if tag == 0xA0:                                 # [0] version
        tag, value, end = _tlv(der, end)
    for _ in range(2):                              # past serialNumber: signature, issuer
        tag, value, end = _tlv(der, end)
    tag, value, _ = _tlv(der, end)                  # validity
    if tag != 0x30:
        raise CheckError('malformed certificate validity')
    _, _, after_start = _tlv(der, value)            # notBefore
    tag, value, end = _tlv(der, after_start)        # notAfter
    text = der[value:end].decode('ascii')
    if tag == 0x17:                                 # UTCTime YYMMDDHHMMSSZ
        moment = datetime.datetime.strptime(text, '%y%m%d%H%M%SZ')
    elif tag == 0x18:                               # GeneralizedTime YYYYMMDDHHMMSSZ
        moment = datetime.datetime.strptime(text, '%Y%m%d%H%M%SZ')
    else:
        raise CheckError('malformed certificate notAfter')
    return moment.replace(tzinfo=datetime.timezone.utc)


def add_months(moment, months):
    month = moment.month - 1 + months
    year, month = moment.year + month // 12, month % 12 + 1
    days = [31, 29 if year % 4 == 0 and (year % 100 or year % 400 == 0) else 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    return moment.replace(year=year, month=month, day=min(moment.day, days[month - 1]))


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def variants(directory=SHORTCUTS):
    """(source, signed) pairs, one per tile colour: every OinkCopy-<colour>.unsigned.plist
    and the OinkCopy-<colour>.shortcut signed from it."""
    sources = sorted(directory.glob('OinkCopy-*.unsigned.plist'))
    if not sources:
        raise CheckError(f'no OinkCopy-*.unsigned.plist in {directory}')
    return [(source, source.with_name(source.name.replace('.unsigned.plist', '.shortcut'))) for source in sources]


def check_shortcut(now=None, months=3, directory=SHORTCUTS):
    """Raises CheckError unless every signed variant is the one recorded for its current
    source, nothing else is recorded, and every leaf certificate outlives `now` + `months`.
    Returns the earliest notAfter."""
    now = now or datetime.datetime.now(datetime.timezone.utc)
    record = json.loads((directory / MANIFEST_NAME).read_text())
    pairs = variants(directory)
    extra = set(record) - {signed.name for _, signed in pairs}
    if extra:
        raise CheckError(f'{MANIFEST_NAME} records {", ".join(sorted(extra))} without a source: run sign-shortcut.sh')
    earliest = None
    for source, signed in pairs:
        entry = record.get(signed.name, {})
        if entry.get('source_sha256') != sha256(source):
            raise CheckError(f'{source.name} changed since {signed.name} was signed: run sign-shortcut.sh')
        if not signed.exists() or entry.get('signed_sha256') != sha256(signed):
            raise CheckError(f'{signed.name} is not the recorded signed file: run sign-shortcut.sh')
        expires = not_after(signing_chain(signed.read_bytes())[0])
        if expires < add_months(now, months):
            raise CheckError(f'{signed.name} signing certificate expires {expires:%Y-%m-%d}, within {months} months: '
                             'run apps/mobile/ios/Shortcuts/sign-shortcut.sh (macOS, iCloud signed in)')
        earliest = expires if earliest is None else min(earliest, expires)
    return earliest


def write_manifest(directory=SHORTCUTS):
    record = {}
    for source, signed in variants(directory):
        expires = not_after(signing_chain(signed.read_bytes())[0])
        record[signed.name] = {
            'source_sha256': sha256(source),
            'signed_sha256': sha256(signed),
            'certificate_not_after': f'{expires:%Y-%m-%dT%H:%M:%SZ}',
        }
    (directory / MANIFEST_NAME).write_text(json.dumps(record, indent=2) + '\n')


def check_extensions(app):
    """Require share, controls and ReplayKit broadcast extensions. Each PlugIns/*.appex
    must carry the app's version/build and an id under the app's identifier."""
    app = Path(app)
    info = plistlib.loads((app / 'Info.plist').read_bytes())
    version, build, bundle = info['CFBundleShortVersionString'], info['CFBundleVersion'], info['CFBundleIdentifier']
    extensions = sorted((app / 'PlugIns').glob('*.appex'))
    if not extensions:
        raise CheckError(f'{app.name} has no app extensions')
    problems = []
    identifiers = set()
    for extension in extensions:
        own = plistlib.loads((extension / 'Info.plist').read_bytes())
        identifiers.add(own.get('CFBundleIdentifier'))
        if (own.get('CFBundleShortVersionString'), own.get('CFBundleVersion')) != (version, build):
            problems.append(f"{extension.name}: {own.get('CFBundleShortVersionString')} ({own.get('CFBundleVersion')}), "
                            f'app {version} ({build})')
        if not str(own.get('CFBundleIdentifier', '')).startswith(bundle + '.'):
            problems.append(f"{extension.name}: bundle id {own.get('CFBundleIdentifier')} is not under {bundle}")
        if own.get('CFBundleIdentifier') == bundle + '.broadcast':
            point = own.get('NSExtension', {})
            if (point.get('NSExtensionPointIdentifier') != 'com.apple.broadcast-services-upload'
                    or point.get('NSExtensionAttributes', {}).get('RPBroadcastProcessMode') != 'RPBroadcastProcessModeSampleBuffer'):
                problems.append(f'{extension.name}: invalid ReplayKit sample-buffer extension')
    for suffix in ('share', 'controls', 'broadcast'):
        if bundle + '.' + suffix not in identifiers:
            problems.append(f'missing extension {bundle}.{suffix}')
    if problems:
        raise CheckError('; '.join(problems))
    return [extension.name for extension in extensions]


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest='command', required=True)
    shortcut = commands.add_parser('shortcut')
    shortcut.add_argument('--months', type=int, default=3)
    commands.add_parser('manifest')
    extensions = commands.add_parser('extensions')
    extensions.add_argument('app')
    args = parser.parse_args(argv)
    try:
        if args.command == 'shortcut':
            print(f'OinkCopy-*.shortcut: certificates valid until {check_shortcut(months=args.months):%Y-%m-%d}')
        elif args.command == 'manifest':
            write_manifest()
            print(f'{MANIFEST_NAME} written')
        else:
            print('extensions match the app:', ', '.join(check_extensions(args.app)))
    except CheckError as error:
        print(f'ios_package: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
