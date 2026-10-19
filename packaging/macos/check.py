#!/usr/bin/env python3
"""Compile both release Swift entry points and optionally run pure provider tests."""
import argparse
from pathlib import Path
import tempfile
from build import I, ROOT, run, write_swift_identity


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--test', action='store_true')
    args = p.parse_args()
    with tempfile.TemporaryDirectory(prefix='oink-swift-') as temporary:
        tmp = Path(temporary)
        generated = tmp / 'ReleaseIdentity.swift'
        write_swift_identity(generated)
        common = ['xcrun', 'swiftc', '-swift-version', '5', '-parse-as-library', '-D', 'OINK_RELEASE',
                  '-target', f'arm64-apple-macos{I["macos_minimum"]}']
        shared = [generated, ROOT / 'apps/file-provider/Shared.swift']
        for entry in ['Extension.swift', 'Manager.swift']:
            flags = (['-application-extension', ROOT / 'apps/file-provider/ReportCapture.swift']
                     if entry == 'Extension.swift' else [ROOT / 'apps/file-provider/Uninstall.swift'])
            run(*common, '-typecheck', *shared, *flags, ROOT / 'apps/file-provider' / entry)
        run('xcrun', 'clang', '-O2', '-std=c11', '-target', f'arm64-apple-macos{I["macos_minimum"]}',
            '-c', ROOT / 'apps/file-provider/CrashCapture.c', '-o', tmp / 'crash-capture.o')
        if args.test:
            for entry in ['Tests.swift', 'EnumeratorTests.swift', 'UninstallTests.swift']:
                extension = [ROOT / 'apps/file-provider/Extension.swift', ROOT / 'apps/file-provider/ReportCapture.swift',
                             tmp / 'crash-capture.o'] if entry == 'EnumeratorTests.swift' else []
                if entry == 'UninstallTests.swift':
                    extension = [ROOT / 'apps/file-provider/Uninstall.swift']
                output = tmp / entry.removesuffix('.swift')
                run(*common, '-D', 'DEBUG', *shared, *extension, ROOT / 'apps/file-provider' / entry, '-o', output)
                run(output)


if __name__ == '__main__':
    main()
