"""Reject known macOS baseline violations before signing or shipping Mach-O code.

This is a targeted deny list, not a complete SDK availability database. SDK
annotations alone are insufficient: IOHIDAccessCheckAuditToken is annotated as
10.15 in the SDK but is missing from IOKit on a real macOS 14.7 installation.
"""
import argparse
from pathlib import Path
import re
import subprocess


MINIMUM_OS = '14.2'
UNAVAILABLE_SYMBOLS = {'_IOHIDAccessCheckAuditToken'}
MACHO_MAGIC = {
    b'\xfe\xed\xfa\xce', b'\xce\xfa\xed\xfe',
    b'\xfe\xed\xfa\xcf', b'\xcf\xfa\xed\xfe',
    b'\xca\xfe\xba\xbe', b'\xbe\xba\xfe\xca',
    b'\xca\xfe\xba\xbf', b'\xbf\xba\xfe\xca',
}


def check_imports(output, image, minimum_os=MINIMUM_OS):
    if minimum_os != MINIMUM_OS:
        raise RuntimeError('Review the Mach-O import deny list for the new deployment target')
    for line in output.splitlines():
        if '(undefined)' not in line or re.search(r'\bweak\b', line):
            continue
        for symbol in UNAVAILABLE_SYMBOLS:
            if re.search(r'(?<!\S)' + re.escape(symbol) + r'(?!\S)', line):
                raise RuntimeError(f'{image}: strong import {symbol} is unavailable on macOS {minimum_os}; '
                                   'use an API available at the deployment target or a guarded weak import')


def verify_imports(paths, minimum_os=MINIMUM_OS):
    """Scan every image, including nested frameworks, helpers and XPC services."""
    images = []
    for path in paths:
        path = Path(path)
        if not path.exists():
            raise FileNotFoundError(path)
        for image in sorted(path.rglob('*')) if path.is_dir() else [path]:
            if image.is_symlink() or not image.is_file():
                continue
            with image.open('rb') as stream:
                if stream.read(4) not in MACHO_MAGIC:
                    continue
            output = subprocess.check_output(['xcrun', 'nm', '-m', '-u', str(image)], text=True)
            check_imports(output, image, minimum_os)
            images.append(image)
    if not images:
        raise RuntimeError('No Mach-O images found for the deployment-target import check')
    return images


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('paths', type=Path, nargs='+', help='Binaries or assembled app directories; no signing required')
    parser.add_argument('--minimum-os', default=MINIMUM_OS)
    args = parser.parse_args()
    images = verify_imports(args.paths, args.minimum_os)
    print(f'macOS {args.minimum_os} known-symbol import check passed: {len(images)} Mach-O images')


if __name__ == '__main__':
    main()
