import sys
from pathlib import Path
ROOT = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(ROOT / "build-support"), str(ROOT / "build-support/update")]
import importlib.util
from pathlib import Path
import unittest
import contextlib
import io
import json
import sys
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('release_ci', Path(__file__).parents[2] / 'build-support/ci.py')
CI = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CI)


class AffectedSelection(unittest.TestCase):
    def test_full_ci_runs_integration_tests_and_release_library_tests(self):
        with patch.object(sys, 'argv', ['ci.py']), \
                patch.object(CI.subprocess, 'check_output', return_value=json.dumps(self.metadata())), \
                patch.object(CI.subprocess, 'run') as run, \
                patch.object(CI.platform, 'system', return_value='Windows'), \
                contextlib.redirect_stdout(io.StringIO()):
            CI.main()
        commands = [call.args[0] for call in run.call_args_list]
        self.assertIn(['cargo', 'test', '--locked', '--workspace'], commands)
        self.assertIn(['cargo', 'test', '--locked', '--release', '--lib', '--workspace'], commands)
        self.assertIn(['cargo', 'clippy', '--locked', '--all-targets', '--workspace', '--', '-D', 'warnings'], commands)
        for call in run.call_args_list:
            environment = call.kwargs['env']
            if '--release' in call.args[0]:
                self.assertEqual(environment['CARGO_PROFILE_RELEASE_LTO'], 'false')
                self.assertEqual(environment['CARGO_PROFILE_RELEASE_CODEGEN_UNITS'], '16')

    def metadata(self):
        return dict(workspace_members=['core', 'agent', 'ui', 'other'], packages=[
            dict(id=name, name=name, manifest_path=str(CI.ROOT / path / 'Cargo.toml'), dependencies=[dict(name=d) for d in deps])
            for name, path, deps in [('core', 'crates/core', []), ('agent', 'crates/agent', ['core']),
                                     ('ui', 'apps/oink/src-tauri', ['agent']), ('other', 'crates/other', [])]])

    def test_transitive_reverse_dependencies(self):
        self.assertEqual(CI.selected_packages(self.metadata(), ['crates/core/src/deleted.rs']), ['agent', 'core', 'ui'])

    def test_frontend_is_embedded_in_ui(self):
        self.assertEqual(CI.selected_packages(self.metadata(), ['apps/oink/src/example.ts']), ['ui'])

    def test_dependency_and_unknown_build_changes_select_full(self):
        for path in ['Cargo.lock', 'build-support/build.py', '.cargo/config.toml', 'crates/removed/Cargo.toml']:
            self.assertEqual(CI.selected_packages(self.metadata(), [path]), ['agent', 'core', 'other', 'ui'])

    def test_docs_do_not_trigger_rust(self):
        self.assertEqual(CI.selected_packages(self.metadata(), ['docs/example.md']), [])


if __name__ == '__main__':
    unittest.main()
