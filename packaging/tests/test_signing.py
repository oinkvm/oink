import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('mac_signing', Path(__file__).parents[1] / 'macos/signing.py')
S = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(S)


class MacSigning(unittest.TestCase):
    def test_ticket_service_wait_preserves_stapled_app_and_rechecks_validation(self):
        results = [subprocess.CompletedProcess([], code, '', '') for code in [0, 68, 0]]
        with patch.object(S, 'private_run', side_effect=results) as run, patch.object(S.time, 'sleep') as sleep:
            S.staple_and_validate(Path('Oink.app'))
        self.assertEqual([c.args[2] for c in run.call_args_list], ['staple', 'validate', 'validate'])
        self.assertTrue(all(c.args[3] == Path('Oink.app') for c in run.call_args_list))
        sleep.assert_called_once_with(2)

    def test_invalid_and_revoked_tickets_are_never_retried(self):
        for code in [65, 66, 73, 77]:
            with self.subTest(code=code), patch.object(S, 'private_run', return_value=subprocess.CompletedProcess([], code, 'private', 'private')) as run, patch.object(S.time, 'sleep') as sleep:
                with self.assertRaisesRegex(RuntimeError, f'exit {code}') as error:
                    S.ticket_operation('validate', Path('Oink.app'))
                self.assertNotIn('private', str(error.exception))
                self.assertEqual(run.call_count, 1)
                sleep.assert_not_called()

    def test_ticket_service_wait_has_a_fixed_deadline(self):
        now = [0]
        def pause(seconds):
            now[0] += seconds
        with patch.object(S.time, 'monotonic', side_effect=lambda: now[0]), patch.object(S.time, 'sleep', side_effect=pause), patch.object(S, 'private_run', return_value=subprocess.CompletedProcess([], 68, '', '')) as run:
            with self.assertRaisesRegex(RuntimeError, 'after 5s'):
                S.ticket_operation('validate', Path('Oink.app'), timeout=5)
        self.assertEqual(now[0], 5)
        self.assertEqual(run.call_count, 2)

    def test_only_developer_id_for_requested_team_can_be_selected(self):
        fingerprint = 'A' * 40
        entries = '\n'.join([
            f'1) {fingerprint} "Developer ID Application: Fixture (893V37X2CW)"',
            f'2) {"B" * 40} "Apple Development: Fixture (893V37X2CW)"',
            f'3) {"C" * 40} "Developer ID Application: Fixture (OTHERTEAM0)"'])
        with patch.object(S, 'private_run', return_value=subprocess.CompletedProcess([], 0, entries, '')):
            self.assertEqual(S.select_identity('893V37X2CW'), fingerprint)
            with self.assertRaises(RuntimeError):
                S.select_identity('893V37X2CW', 'C' * 40)

    def test_failure_does_not_echo_certificate_or_credentials(self):
        output = 'sensitive command output'
        with patch.object(S.subprocess, 'run', return_value=subprocess.CompletedProcess([], 1, output, output)):
            with self.assertRaises(RuntimeError) as error:
                S.private_run('codesign', '--sign', output)
        self.assertNotIn(output, str(error.exception))

    def test_accepted_zip_is_not_stapled_as_a_zip(self):
        results = [subprocess.CompletedProcess([], 0, '{"id":"00000000-0000-0000-0000-000000000001"}', ''),
                   subprocess.CompletedProcess([], 0, '{"status":"Accepted"}', '')]
        with patch.object(S, 'private_run', side_effect=results) as run:
            record = S.notarize(Path('submission.zip'), 'profile', None, 1)
        self.assertEqual(record['status'], 'Accepted')
        self.assertEqual(run.call_count, 2)

    def test_notary_timeout_retains_submission_id(self):
        results = [subprocess.CompletedProcess([], 0, '{"id":"00000000-0000-0000-0000-000000000001"}', ''),
                   subprocess.TimeoutExpired('notarytool', 1)]
        with patch.object(S, 'private_run', side_effect=results):
            record = S.notarize(Path('package.dmg'), 'profile', None, 1)
        self.assertEqual(record['status'], 'In Progress')
        self.assertEqual(record['id'], '00000000-0000-0000-0000-000000000001')
