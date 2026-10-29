import ast
import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).parents[2]
SPEC = importlib.util.spec_from_file_location('awake_mac_signing', ROOT / 'packaging/macos/signing.py')
SIGNING = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SIGNING)


class SigningAwake(unittest.TestCase):
    def test_display_and_idle_assertions_cover_normal_and_failed_operations(self):
        for fail in (False, True):
            with self.subTest(fail=fail):
                process = Mock()
                process.poll.return_value = None
                with patch.object(SIGNING.subprocess, 'Popen', return_value=process) as start:
                    try:
                        with SIGNING.awake_for_signing():
                            start.assert_called_once_with([
                                '/usr/bin/caffeinate', '-d', '-i', '-w', str(SIGNING.os.getpid())])
                            process.terminate.assert_not_called()
                            if fail:
                                raise ValueError('operation failed')
                    except ValueError:
                        self.assertTrue(fail)
                process.terminate.assert_called_once_with()
                process.wait.assert_called_once_with(timeout=5)

    def test_dead_assertion_process_prevents_unprotected_operations(self):
        process = Mock()
        process.poll.return_value = 1
        entered = False
        with patch.object(SIGNING.subprocess, 'Popen', return_value=process):
            with self.assertRaisesRegex(RuntimeError, 'display awake'):
                with SIGNING.awake_for_signing():
                    entered = True
        self.assertFalse(entered)
        process.terminate.assert_not_called()

    def test_assertions_span_signed_smoke_and_both_notarizations(self):
        tree = ast.parse((ROOT / 'packaging/macos/build.py').read_text())
        guards = [node for node in ast.walk(tree) if isinstance(node, ast.With)
                  and any(isinstance(item.context_expr, ast.Call)
                          and isinstance(item.context_expr.func, ast.Name)
                          and item.context_expr.func.id == 'awake_for_signing'
                          for item in node.items)]
        self.assertEqual(len(guards), 1)
        calls = [node.func.id for node in ast.walk(guards[0]) if isinstance(node, ast.Call)
                 and isinstance(node.func, ast.Name)]
        self.assertIn('launch_smoke', calls)
        self.assertEqual(calls.count('notarize'), 2)
        self.assertIn('staple_and_validate', calls)
        self.assertIn('verify_app', calls)
        self.assertNotIn('compile_provider_extension', calls)
