"""Cross-language contract for the protected MSI authorization record."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]


class WindowsAdmission(unittest.TestCase):
    def test_transaction_worker_location_is_not_msi_admission_authority(self):
        worker = (ROOT / 'crates/oink-winsvc/src/platform/update/worker.rs').read_text()
        common = (ROOT / 'crates/oink-winsvc/src/platform/update/mod.rs').read_text()
        maintenance = (ROOT / 'packaging/windows/Maintenance.h').read_text()
        # Real production relationship that previously disagreed: Rust uses
        # maintenance/transactions/<tx>/worker.exe, the CA expected maintenance/worker.exe.
        self.assertIn('.join("transactions")\n        .join(tx)', common)
        self.assertIn('let worker = directory.join("worker.exe");', worker)
        for removed in ('ID_COPIED_WORKER', 'worker_sha256', 'worker_pid', 'pending_sha256',
                        'DuplicateHandle', 'start_guard', 'nested_authorized'):
            self.assertNotIn(removed, maintenance)
        authorization = re.search(r'struct Authorization.*?\{(.*?)\n        }', worker, re.S).group(1)
        fields = set(re.findall(r'^\s*(\w+):', authorization, re.M))
        self.assertEqual(fields, {'schema', 'product_code', 'package_sha256',
                                 'remove_product_code', 'remove_package_sha256'})
        self.assertIn('admission::matches(record, code, hash, remove, remove_hash)', maintenance)

    def test_worker_and_post_stop_ca_cover_the_same_product_roles(self):
        identity = __import__('json').loads((ROOT / 'packaging/identity.json').read_text())
        worker = (ROOT / 'crates/oink-winsvc/src/platform/update/worker.rs').read_text()
        ca = (ROOT / 'packaging/windows/InstallerCA.cpp').read_text()
        scan = worker[worker.index('fn product_processes('):worker.index('fn inherited_lock(')]
        binaries = {identity['binaries'][r] + '.exe' for r in ('ui', 'agent', 'service', 'crash_collector')}
        self.assertEqual(set(re.findall(r'"(oink[\w.-]*\.exe)"', scan)), binaries)
        self.assertIn('{ID_UI, ID_AGENT, ID_SERVICE_EXE, ID_CRASH_COLLECTOR}', ca)
        preflight = ca[ca.index('UINT __stdcall CheckProtectedState('):ca.index('UINT __stdcall CleanupMaintenance(')]
        self.assertNotIn('clients_quiescent', preflight)
        package = (ROOT / 'packaging/windows/Package.wxs').read_text()
        self.assertIn('Action="CheckQuiescence" After="StopServices"', package)
        self.assertNotIn('MaintenanceGuard', package)
