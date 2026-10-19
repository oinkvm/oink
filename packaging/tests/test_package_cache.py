"""Package cache configuration must not redirect or stop the dev cache server."""
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support'))
from package_cache import configure_cache


class PackageCache(unittest.TestCase):
    def test_separate_defaults_on_each_host(self):
        for system, size in [('Darwin', '12G'), ('Windows', '80G')]:
            with self.subTest(system=system), patch('package_cache.platform.system', return_value=system), \
                    patch('package_cache.shutil.which', return_value='/tools/sccache'):
                original = {'SCCACHE_DIR': '/dev-cache', 'SCCACHE_CACHE_SIZE': '40G',
                            'SCCACHE_SERVER_PORT': '4226', 'SCCACHE_SERVER_UDS': '/dev.sock'}
                result = configure_cache(original)
                self.assertEqual(result['SCCACHE_SERVER_PORT'], '4227')
                self.assertEqual(result['SCCACHE_CACHE_SIZE'], size)
                self.assertEqual(Path(result['SCCACHE_DIR']).name, 'oink-package-sccache')
                self.assertNotEqual(result['SCCACHE_DIR'], original['SCCACHE_DIR'])
                self.assertNotIn('SCCACHE_SERVER_UDS', result)
                self.assertEqual(result['RUSTC_WRAPPER'], '/tools/sccache')
                self.assertEqual(original['SCCACHE_SERVER_PORT'], '4226')
                self.assertEqual(original['SCCACHE_SERVER_UDS'], '/dev.sock')

    def test_package_overrides_are_scoped(self):
        with patch('package_cache.shutil.which', return_value='/tools/sccache'):
            result = configure_cache({'OINK_PACKAGE_SCCACHE_DIR': '/package-cache',
                                      'OINK_PACKAGE_SCCACHE_SIZE': '16G',
                                      'OINK_PACKAGE_SCCACHE_PORT': '4327'})
        self.assertEqual(result['SCCACHE_DIR'], str(Path('/package-cache').resolve()))
        self.assertEqual(result['SCCACHE_CACHE_SIZE'], '16G')
        self.assertEqual(result['SCCACHE_SERVER_PORT'], '4327')

    def test_dev_port_and_invalid_ports_rejected(self):
        with patch('package_cache.shutil.which', return_value='/tools/sccache'):
            for port in ['4226', '0', '65536', '-1', 'x']:
                with self.subTest(port=port), self.assertRaises(ValueError):
                    configure_cache({'OINK_PACKAGE_SCCACHE_PORT': port})

    def test_hosts_without_sccache_still_build(self):
        with patch('package_cache.shutil.which', return_value=None):
            self.assertEqual(configure_cache({'PATH': '/tools'}), {'PATH': '/tools'})
