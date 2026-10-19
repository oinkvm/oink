"""Deployment-target import regressions; compiled fixtures are never launched."""
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support'))
import macos_imports as M


class MacImports(unittest.TestCase):
    def test_strong_import_fails_and_weak_import_is_allowed(self):
        symbol = '_IOHIDAccessCheckAuditToken'
        with self.assertRaisesRegex(RuntimeError, 'strong import.*unavailable on macOS 14.2'):
            M.check_imports(f'                 (undefined) external {symbol} (from IOKit)', 'backend')
        M.check_imports(f'                 (undefined) weak external {symbol} (from IOKit)', 'backend')
        M.check_imports('                 (undefined) external _IOHIDCheckAccess (from IOKit)', 'backend')
        M.check_imports(f'0000000000001000 (__TEXT,__text) external {symbol}', 'backend')
        M.check_imports(f'                 (undefined) external {symbol}Other (from IOKit)', 'backend')

    def test_baseline_change_requires_review(self):
        with self.assertRaisesRegex(RuntimeError, 'Review.*deny list'):
            M.check_imports('', 'backend', '15.0')

    def test_all_nested_images_are_checked_without_signing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            images = [root / 'Contents/MacOS/backend',
                      root / 'Contents/Frameworks/Test.framework/Versions/A/Test',
                      root / 'Contents/Frameworks/Test.framework/Versions/A/XPCServices/Test.xpc/Contents/MacOS/Test']
            for image in images:
                image.parent.mkdir(parents=True, exist_ok=True)
                image.write_bytes(b'\xcf\xfa\xed\xfe' + b'fixture')
            (root / 'Contents/Info.plist').write_text('not a Mach-O')
            if platform.system() != 'Windows':
                (root / 'Contents/MacOS/link').symlink_to(images[0])
            with patch.object(M.subprocess, 'check_output', return_value='') as nm:
                self.assertEqual(set(M.verify_imports([root])), set(images))
                self.assertEqual({Path(call.args[0][-1]) for call in nm.call_args_list}, set(images))
            with patch.object(M.subprocess, 'check_output',
                              return_value='(undefined) external _IOHIDAccessCheckAuditToken (from IOKit)'):
                with self.assertRaisesRegex(RuntimeError, 'strong import'):
                    M.verify_imports([images[1]])

    def test_missing_images_and_tool_errors_cannot_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaises(FileNotFoundError):
                M.verify_imports([root / 'missing'])
            with self.assertRaisesRegex(RuntimeError, 'No Mach-O'):
                M.verify_imports([root])
            image = root / 'binary'
            image.write_bytes(b'\xcf\xfa\xed\xfe')
            with patch.object(M.subprocess, 'check_output', side_effect=subprocess.CalledProcessError(1, 'nm')):
                with self.assertRaises(subprocess.CalledProcessError):
                    M.verify_imports([image])

    @unittest.skipUnless(platform.system() == 'Darwin', 'requires Apple linker and IOKit SDK')
    def test_real_macho_strong_import_fails_weak_import_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / 'main.c'
            for weak in [False, True]:
                attribute = '__attribute__((weak_import))' if weak else ''
                source.write_text('typedef struct { unsigned int words[8]; } token_t;\n'
                                  f'extern _Bool IOHIDAccessCheckAuditToken(unsigned int, token_t) {attribute};\n'
                                  'int main(void) { return IOHIDAccessCheckAuditToken '
                                  '? IOHIDAccessCheckAuditToken(1, (token_t){{0}}) : 0; }\n')
                image = root / ('weak' if weak else 'strong')
                subprocess.run(['xcrun', 'clang', '-target', 'arm64-apple-macos14.2', str(source),
                                '-framework', 'IOKit', '-o', str(image)], check=True, capture_output=True)
                if weak:
                    self.assertEqual(M.verify_imports([image]), [image])
                else:
                    with self.assertRaisesRegex(RuntimeError, 'strong import'):
                        M.verify_imports([image])


if __name__ == '__main__':
    unittest.main()
