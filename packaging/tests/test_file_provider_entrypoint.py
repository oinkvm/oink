"""Link the real bootstrap against a bounded ExtensionKit callback model."""
import importlib.util
import os
from pathlib import Path
import platform
import select
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(ROOT / 'packaging/macos'), str(ROOT / 'build-support')]
SPEC = importlib.util.spec_from_file_location('provider_build', ROOT / 'packaging/macos/build.py')
BUILD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BUILD)


def run_entrypoint(image, args, *, env=None):
    # On macOS 26.6.2, syspolicyd held the freshly linked fixture behind
    # concurrent Cargo assessments for >5s before allowing user code to run.
    # The C constructor acknowledges dyld/startup, then waits for us to release
    # the real entry. Keep its 5s execution budget; a separate bounded startup
    # wait needs neither a signing identity nor a warm-up that could hide bugs.
    ready, signal = os.pipe()
    try:
        with subprocess.Popen([image, *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True, pass_fds=(signal,),
                              env={**(os.environ if env is None else env),
                                   'PROVIDER_TEST_READY_FD': str(signal)}) as process:
            os.close(signal)
            signal = None
            try:
                if not select.select([ready], [], [], 60)[0]:
                    raise AssertionError('fixture did not finish macOS startup assessment within 60s')
                if os.read(ready, 1) != b'R':
                    raise AssertionError('fixture exited before its pre-entry handshake')
                stdout, stderr = process.communicate(input='G', timeout=5)
                return subprocess.CompletedProcess(process.args, process.returncode, stdout, stderr)
            finally:
                if process.poll() is None:
                    process.kill()
                process.communicate()
    finally:
        os.close(ready)
        if signal is not None:
            os.close(signal)


@unittest.skipUnless(platform.system() == 'Darwin', 'requires the Apple linker and Swift SDK')
class ProviderEntrypoint(unittest.TestCase):
    def test_runtime_callback_does_not_reenter_bootstrap_and_cleanup_is_separate(self):
        with tempfile.TemporaryDirectory(prefix='provider-entry-') as temporary:
            tmp = Path(temporary)
            runtime = tmp / 'runtime.c'
            # macOS 26's _EXUtil reads the relative Swift entry pointer from
            # __TEXT,__swift5_entry before calling main inside NSExtensionMain.
            # Bound reentry to fail deterministically without starting any OS
            # extension, registering a domain or accessing a real container.
            runtime.write_text('''#include <mach-o/getsect.h>
#include <mach-o/ldsyms.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
// Runs before the linker's real OinkFileProviderMain entry, without invoking it.
__attribute__((constructor)) static void await_entry_test(void) {
    const char *value = getenv("PROVIDER_TEST_READY_FD");
    if (!value) _exit(88);
    int fd = atoi(value);
    if (write(fd, "R", 1) != 1) _exit(89);
    close(fd);
    char go;
    if (read(STDIN_FILENO, &go, 1) != 1 || go != 'G') _exit(90);
}
int NSExtensionMain(int argc, char **argv) {
    static int calls;
    if (++calls > 1) { puts("recursive-bootstrap"); return 87; }
    unsigned long size = 0;
    const uint8_t *entry = getsectiondata(&_mh_execute_header, "__TEXT", "__swift5_entry", &size);
    if (entry && size >= sizeof(int32_t)) {
        int32_t offset;
        memcpy(&offset, entry, sizeof(offset));
        if (offset) {
            int (*callback)(int, char **) = (void *)(entry + offset);
            return callback(argc, argv);
        }
    }
    printf("runtime:%d\\n", argc);
    return 0;
}
''')
            cleanup = tmp / 'cleanup.swift'
            cleanup.write_text('''import Foundation
func clearOwnProviderContainer() throws {
    if ProcessInfo.processInfo.environment["PROVIDER_TEST_CLEANUP_FAIL"] == "1" {
        throw CocoaError(.fileWriteNoPermission)
    }
    print("cleanup")
}
''')
            obj = tmp / 'runtime.o'
            target = f'arm64-apple-macos{BUILD.I["macos_minimum"]}'
            subprocess.run(['xcrun', 'clang', '-target', target, '-c', runtime, '-o', obj],
                           check=True, capture_output=True)
            image = tmp / 'bootstrap'
            subprocess.run(['xcrun', 'swiftc', '-swift-version', '5', '-O', '-parse-as-library',
                            '-target', target, *BUILD.PROVIDER_EXTENSION_FLAGS,
                            ROOT / 'apps/file-provider/ExtensionMain.swift', cleanup, obj, '-o', image],
                           check=True, capture_output=True)
            for args, expected in [([], 'runtime:1\n'),
                                   (['-LaunchArguments', 'fixture'], 'runtime:3\n'),
                                   (['clear-uninstall-data'], 'cleanup\n'),
                                   (['clear-uninstall-data', 'extra'], 'runtime:3\n')]:
                with self.subTest(args=args):
                    result = run_entrypoint(image, args)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertEqual(result.stdout, expected)
            result = run_entrypoint(image, ['clear-uninstall-data'],
                                    env={**os.environ, 'PROVIDER_TEST_CLEANUP_FAIL': '1'})
            self.assertEqual(result.returncode, 1)


if __name__ == '__main__':
    unittest.main()
