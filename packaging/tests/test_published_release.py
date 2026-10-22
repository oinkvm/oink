import copy
import importlib.util
import json
import os
from pathlib import Path
import re
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support/update'))
import feed
import mac_release


class PublishedFeedContract(unittest.TestCase):
    def test_release_published_event_triggers_no_workflow_or_feed_dispatch(self):
        workflow = (ROOT / '.github/workflows/release.yml').read_text()
        triggers = workflow.split('on:\n', 1)[1].split('permissions:\n', 1)[0]
        self.assertEqual(re.findall(r'^  ([a-z_]+):', triggers, re.M), ['workflow_dispatch'])
        self.assertNotIn('publish-updates:', workflow)
        self.assertNotIn('uses: ./.github/workflows/publish-', workflow)
        self.assertNotIn('gh workflow run', workflow)
        self.assertNotIn('actions: write', workflow)
        for path in (ROOT / '.github/workflows').glob('*.yml'):
            self.assertNotRegex(path.read_text(), r'(?m)^  release:', path.name)

    def test_manual_publishers_pin_notes_and_code_to_dispatch_default_branch_commit(self):
        for filename in ['seed-update-feeds.yml', 'publish-macos-feed.yml', 'publish-windows-feed.yml']:
            workflow = (ROOT / '.github/workflows' / filename).read_text()
            self.assertIn("github.event_name == 'workflow_dispatch'", workflow)
            self.assertIn("github.ref == format('refs/heads/{0}', github.event.repository.default_branch)", workflow)
            refs = re.findall(r'^          ref: (.*)$', workflow, re.M)
            self.assertIn('${{ github.sha }}', refs)
            self.assertTrue(all(ref in ('${{ github.sha }}', 'update-feed-state') for ref in refs))
            self.assertEqual(workflow.count('uses: actions/checkout@'), len(refs))

    def test_recovery_can_publish_only_the_uncommitted_platform(self):
        workflow = (ROOT / '.github/workflows/seed-update-feeds.yml').read_text()
        self.assertIn('default: both', workflow)
        self.assertIn('options: [both, mac, windows]', workflow)
        self.assertIn('case "$PLATFORM" in both|mac|windows)', workflow)
        for platform, excluded in [('macos', 'windows'), ('windows', 'mac')]:
            job = re.search(r'^  ' + platform + r':\n((?:    .*\n)+)', workflow, re.M)[1]
            self.assertIn('needs: published', job)
            self.assertIn("if: inputs.platform != '" + excluded + "'", job)

    def test_platform_deployments_do_not_collide_in_same_run_artifacts(self):
        workflow = (ROOT / '.github/workflows/deploy-feed.yml').read_text()
        self.assertIn("github.event_name != 'release'", workflow)
        self.assertNotIn("github.event_name == 'workflow_dispatch'", workflow)
        for prefix in ('previous-feed', 'next-feed'):
            self.assertIn('name: ' + prefix + '-${{ inputs.platform }}', workflow)
            self.assertIn('artifact_name: ' + prefix + '-${{ inputs.platform }}', workflow)

    def test_every_publisher_caller_forwards_required_environment_key(self):
        for platform, key in [('macos', 'SPARKLE_ED25519_PRIVATE_KEY'),
                              ('windows', 'WINDOWS_UPDATE_ED25519_PEM_WIN_2026_01')]:
            target = f'publish-{platform}-feed.yml'
            reusable = (ROOT / '.github/workflows' / target).read_text()
            declaration = re.search(r'^      ' + key + r':\n((?:        .*\n)+)', reusable, re.M)
            self.assertIsNotNone(declaration)
            self.assertIn('required: true', declaration[1])
            callers = []
            for path in (ROOT / '.github/workflows').glob('*.yml'):
                for job in re.split(r'(?=^  [A-Za-z0-9_-]+:\n)', path.read_text(), flags=re.M):
                    if f'uses: ./.github/workflows/{target}' in job:
                        callers.append(path.name)
                        self.assertIn(key + ': ${{ secrets.' + key + ' }}', job, path.name)
            self.assertEqual(callers, ['seed-update-feeds.yml'])

    def test_empty_environment_keys_fail_before_invoking_crypto(self):
        with patch.dict(os.environ, {mac_release.SECRET: ''}), patch.object(mac_release, 'command') as command:
            with self.assertRaisesRegex(ValueError, 'sparkle_secret_unavailable'):
                mac_release.sign('unused', Path('unused'), 'unused')
            command.assert_not_called()
        with patch.dict(os.environ, {feed.KEY_ENV['win-2026-01']: ''}), patch.object(feed, 'openssl') as openssl:
            with self.assertRaisesRegex(ValueError, 'secret_unavailable'):
                feed.sign(b'data', 'win-2026-01', 'unused')
            openssl.assert_not_called()


class PublishedReleaseGuard(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location('published_release_guard', ROOT / 'build-support/update/published_release.py')
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.path = Path(temporary.name) / 'event.json'
        self.live = dict(id=7, tag_name='v0.1.1', draft=False, prerelease=False, published_at='2026-10-20T20:04:19Z')
        self.event = dict(repository=dict(default_branch='main'), inputs=dict(release_tag='v0.1.1'))
        environment = patch.dict(os.environ, dict(GITHUB_ACTIONS='true', GITHUB_REPOSITORY='oinkvm/oink',
            GITHUB_EVENT_NAME='workflow_dispatch', GITHUB_REF='refs/heads/main', GITHUB_EVENT_PATH=str(self.path)))
        environment.start()
        self.addCleanup(environment.stop)

    def verify(self, tag='v0.1.1'):
        self.path.write_text(json.dumps(self.event))
        return self.module.published(tag, request=lambda path: copy.deepcopy(self.live))

    def test_main_dispatch_verifies_current_published_stable_release(self):
        self.assertEqual(self.verify(), self.live)
        for field, value in [('draft', True), ('prerelease', True), ('published_at', None), ('tag_name', 'v0.1.0')]:
            original = self.live[field]
            self.live[field] = value
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, 'release_not_public'):
                self.verify()
            self.live[field] = original

    def test_dispatch_rejects_other_refs_repositories_events_and_tag_inputs(self):
        for field, value in [('GITHUB_REF', 'refs/tags/v0.1.1'), ('GITHUB_REF', 'refs/heads/topic'),
                             ('GITHUB_REPOSITORY', 'other/repository'), ('GITHUB_EVENT_NAME', 'pull_request')]:
            with patch.dict(os.environ, {field: value}), self.assertRaisesRegex(ValueError, 'published_release_required'):
                self.verify()
        for tag in ['v0.1.2', 'v0.1.1-beta.1', '../main']:
            with self.subTest(tag=tag), self.assertRaises(ValueError): self.verify(tag)

    def test_release_published_event_is_rejected_before_any_live_request(self):
        self.event.update(action='published', release=copy.deepcopy(self.live))
        feed.write(self.path, self.event)
        with patch.dict(os.environ, GITHUB_EVENT_NAME='release'), patch.object(self.module.subprocess, 'check_output') as request:
            with self.assertRaisesRegex(ValueError, 'published_release_required'):
                self.module.published('v0.1.1')
            request.assert_not_called()

    def test_trusted_notes_validate_channel_severity_and_existing_language_rules(self):
        annotations = dict(channel='stable', severity='critical', notes={'en': 'Final', 'zh-Hans': '最终', 'ja': '最終', 'ko': '최종'})
        for change in [dict(channel='beta'), dict(severity='urgent'), dict(extra=True),
                       dict(notes={'en': 'Missing Chinese'}), dict(notes={**annotations['notes'], 'EN': 'Duplicate'}),
                       dict(notes={**annotations['notes'], 'en': 'bad\x00text'})]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.module.validate_annotations({**annotations, **change})
        self.assertEqual(self.module.validate_annotations(annotations), annotations)
