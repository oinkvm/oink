#!/usr/bin/env python3
"""Compile both release Swift entry points and optionally run pure provider tests."""
import argparse
from pathlib import Path
import tempfile
from build import I, ROOT, run, write_swift_identity, compile_provider_extension


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
        shared = [generated, ROOT / 'apps/file-provider/Shared.swift',
                  ROOT / 'apps/file-provider/Upgrade.swift', ROOT / 'apps/file-provider/ContainerCleanup.swift']
        for entry in ['Extension.swift', 'Manager.swift']:
            flags = (['-application-extension', ROOT / 'apps/file-provider/ReportCapture.swift']
                     if entry == 'Extension.swift' else [ROOT / 'apps/file-provider/Uninstall.swift', ROOT / 'apps/file-provider/NativeReceived.swift'])
            run(*common, '-typecheck', *shared, *flags, ROOT / 'apps/file-provider' / entry)
        run('xcrun', 'clang', '-O2', '-std=c11', '-target', f'arm64-apple-macos{I["macos_minimum"]}',
            '-c', ROOT / 'apps/file-provider/CrashCapture.c', '-o', tmp / 'crash-capture.o')
        # Exercise the release link, including its bootstrap, rather than only
        # typechecking callbacks. ExtensionKit reads __swift5_entry and calls
        # Swift main; an @main bootstrap recursively reenters NSExtensionMain.
        image = tmp / 'provider-extension'
        compile_provider_extension([*common, '-O'], generated, tmp / 'crash-capture.o', image)
        sections = run('otool', '-l', image, capture_output=True, text=True).stdout
        if any(line.split() == ['sectname', '__swift5_entry'] for line in sections.splitlines()):
            raise RuntimeError('File Provider bootstrap must not contain a Swift entry callback: ExtensionKit reenters it')
        symbols = set(run('nm', '-jU', image, capture_output=True, text=True).stdout.splitlines())
        if '_main' in symbols:
            raise RuntimeError('File Provider bootstrap must not export main: ExtensionKit reenters it')
        if '_OinkFileProviderMain' not in symbols:
            raise RuntimeError('File Provider bootstrap entry is missing from the release image')
        if args.test:
            for entry in ['UpgradeTests.swift', 'Tests.swift', 'EnumeratorTests.swift', 'UninstallTests.swift', 'ReceivedTests.swift', 'NativeReceivedTests.swift']:
                extension = [ROOT / 'apps/file-provider/Extension.swift', ROOT / 'apps/file-provider/ReportCapture.swift',
                             tmp / 'crash-capture.o'] if entry == 'EnumeratorTests.swift' else []
                if entry == 'UninstallTests.swift':
                    extension = [ROOT / 'apps/file-provider/Uninstall.swift']
                if entry == 'NativeReceivedTests.swift':
                    extension = [ROOT / 'apps/file-provider/NativeReceived.swift']
                output = tmp / entry.removesuffix('.swift')
                run(*common, '-D', 'DEBUG', *shared, *extension, ROOT / 'apps/file-provider' / entry, '-o', output)
                run(output)


if __name__ == '__main__':
    main()
