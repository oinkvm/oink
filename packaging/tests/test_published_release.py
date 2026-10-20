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
    def test_platform_deployments_do_not_collide_in_same_run_artifacts(self):
        workflow = (ROOT / '.github/workflows/deploy-feed.yml').read_text()
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
            self.assertIn('release.yml', callers)
            self.assertIn('seed-update-feeds.yml', callers)

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

    def test_release_event_remains_bound_to_live_release_identity(self):
        self.event.update(action='published', release=copy.deepcopy(self.live))
        with patch.dict(os.environ, GITHUB_EVENT_NAME='release'):
            self.assertEqual(self.verify(), self.live)
            self.live['id'] += 1
            with self.assertRaisesRegex(ValueError, 'release_changed'): self.verify()
