"""Report checks must use the packaged CLI's authenticated native IPC."""
import importlib.util
import json
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support'))
spec = importlib.util.spec_from_file_location('report_probe', ROOT / 'build-support/check-report-configuration.py')
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class ReportProbe(unittest.TestCase):
    def invoke(self, status):
        child = unittest.mock.Mock()
        child.poll.return_value = None
        child.wait.return_value = 0
        homes = []

        def start(command, *, env, **kwargs):
            home = Path(env['OINK_HOME'])
            homes.append(home)
            # A native endpoint deliberately has no legacy TCP port field.
            (home / 'ipc.json').write_text(json.dumps({'path': str(home / 'ipc.sock')}))
            (home / 'launch-smoke-ready').touch()
            for name in ('OINK_REPORT_DSN', 'OINK_REPORT_REGION', 'OINK_IPC_PORT'):
                self.assertNotIn(name, env)
            return child

        with patch.dict(os.environ, {'OINK_REPORT_DSN': 'fixture-only', 'OINK_REPORT_REGION': 'fixture-only', 'OINK_IPC_PORT': '1'}):
            with patch.object(probe.subprocess, 'Popen', side_effect=start):
                with patch.object(probe.subprocess, 'check_output', return_value=json.dumps(status)) as call:
                    try:
                        probe.mac_status(Path('package/Oink.app'), {'version': '0.3.3', 'build': 100})
                    finally:
                        child.terminate.assert_called_once_with()
                        child.wait.assert_called_once_with(timeout=10)
                    call.assert_called_once()
                    args, kwargs = call.call_args
                    self.assertEqual(args[0], [str(Path('package/Oink.app/Contents/MacOS/oink-agent')), 'call', 'report_status'])
                    self.assertEqual(kwargs['env']['OINK_HOME'], str(homes[0]))
                    self.assertTrue(kwargs['text'])
                    self.assertEqual(kwargs['timeout'], 5)
                    self.assertNotIn('OINK_REPORT_DSN', kwargs['env'])
        self.assertFalse(homes[0].exists())

    def test_native_endpoint_report_check_uses_packaged_cli_without_tcp_port(self):
        self.invoke({'configured': True, 'consent': {'choice': 'unasked'}, 'queued_crashes': 0, 'queued_performance': 0})

    def test_missing_configuration_consent_or_queued_reports_are_not_accepted(self):
        for change in ({'configured': False}, {'consent': {'choice': 'granted'}}, {'queued_crashes': 1}, {'queued_performance': 1}):
            with self.subTest(change=change), self.assertRaises(AssertionError):
                self.invoke({'configured': True, 'consent': {'choice': 'unasked'}, 'queued_crashes': 0, 'queued_performance': 0, **change})
