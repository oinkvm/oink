"""Credential DLL is a signed File-table payload, never a sixth receipt role."""
import copy
import importlib.util
import re
import sys
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path
ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'build-support'))
from identity import signpath_configuration
SPEC = importlib.util.spec_from_file_location('windows_content_credential', ROOT / 'packaging/windows/content.py')
C = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(C)

class CredentialProviderTests(unittest.TestCase):
    def test_signing_includes_provider(self):
        root = ET.fromstring(signpath_configuration())
        images = {e.get('path') for e in root.iter() if e.tag.endswith('}pe-file')}
        self.assertIn('OinkCredentialProvider.dll', images)
        self.assertIn('OinkPasteMenu.dll', images)
        self.assertEqual(sum(name.endswith('.exe') for name in images), 5)

    def test_file_table_and_service_unregister(self):
        source = (ROOT / 'packaging/windows/Package.wxs').read_text()
        self.assertIn('Source="$(var.Payload)/OinkCredentialProvider.dll"', source)
        self.assertIn('Source="$(var.Payload)/OinkPasteMenu.dll"', source)
        ca = (ROOT / 'packaging/windows/InstallerCA.cpp').read_text()
        self.assertIn('L"--unregister-credential-provider"', ca)
        self.assertIn('"--unregister-credential-provider" => auto_unlock::unregister()',
                      (ROOT / 'crates/oink-winsvc/src/platform/mod.rs').read_text())

    def test_unregister_is_a_logging_dll_action(self):
        # Issue #46: an EXE custom action hid the failing step behind "error
        # code 1". The DLL action copies the service image's stderr to the log.
        source = (ROOT / 'packaging/windows/Package.wxs').read_text()
        action = re.search(r'<CustomAction Id="UnregisterCredentialProvider"[^>]*/>', source).group(0)
        for attribute in ('BinaryRef="InstallerCA"', 'DllEntry="UnregisterCredentialProvider"', 'Execute="deferred"',
                          'Impersonate="no"', 'Return="check"'):
            self.assertIn(attribute, action)
        self.assertNotIn('FileRef=', action)
        self.assertNotIn('ExeCommand=', action)
        self.assertIn('''<Custom Action="UnregisterCredentialProvider" Before="RemoveFiles"
              Condition='REMOVE~="ALL" AND NOT UPGRADINGPRODUCTCODE' />''', source)
        ca = (ROOT / 'packaging/windows/InstallerCA.cpp').read_text()
        entry = ca[ca.index('UINT __stdcall UnregisterCredentialProvider'):]
        entry = entry[:entry.index('\n}\n')]
        self.assertIn('STARTF_USESTDHANDLES', ca)
        self.assertIn('hStdError = write', ca)
        self.assertIn('INSTALLMESSAGE_INFO', entry)
        self.assertIn('INSTALLMESSAGE_ERROR', entry)
        self.assertIn('return ERROR_INSTALL_FAILURE;', entry)

    def test_unregister_session_is_not_required(self):
        # Windows Installer runs the action as SYSTEM in the msiexec client's
        # session (1 for a desktop uninstall), so removal must not demand
        # session 0; registration by the service still does.
        native = (ROOT / 'crates/oink-winsvc/native/unlock.cpp').read_text()
        unregister = native[native.index('extern "C" DWORD oink_unlock_unregister(DWORD'):]
        unregister = unregister[:unregister.index('\n}\n')]
        self.assertNotIn('session()', unregister)
        register = native[native.index('extern "C" DWORD oink_unlock_register('):]
        register = register[:register.index('\n}\n')]
        self.assertIn('session() != 0', register)

    def test_msi_tables_require_fatal_system_action_on_standalone_uninstall(self):
        tables = {'CustomAction': [['UnregisterCredentialProvider', '3073', 'InstallerCA', 'UnregisterCredentialProvider']],
                  'InstallExecuteSequence': [['StopServices', '', '1900'], ['CheckQuiescence', '', '1901'],
                                             ['UnregisterCredentialProvider', 'REMOVE~="ALL" AND NOT UPGRADINGPRODUCTCODE', '3499'],
                                             ['RemoveFiles', '', '3500']]}
        C.verify_credential_provider_unregister(tables)
        for kind in ('exe', 'ignore', 'impersonated', 'immediate', 'install', 'upgrade', 'early', 'late', 'ui', 'missing'):
            changed = copy.deepcopy(tables)
            if kind == 'exe': changed['CustomAction'][0][1:4] = ['3090', 'ServiceExe', '--unregister-credential-provider']
            if kind == 'ignore': changed['CustomAction'][0][1] = '3137'
            if kind == 'impersonated': changed['CustomAction'][0][1] = '1025'
            if kind == 'immediate': changed['CustomAction'][0][1] = '1'
            if kind == 'install': changed['InstallExecuteSequence'][2][1] = ''
            if kind == 'upgrade': changed['InstallExecuteSequence'][2][1] = 'REMOVE~="ALL"'
            if kind == 'early': changed['InstallExecuteSequence'][2][2] = '1800'
            if kind == 'late': changed['InstallExecuteSequence'][2][2] = '3600'
            if kind == 'ui': changed['InstallUISequence'] = [['UnregisterCredentialProvider', '', '1000']]
            if kind == 'missing': changed['CustomAction'] = []
            with self.subTest(kind=kind), self.assertRaisesRegex(RuntimeError, 'credential provider removal'):
                C.verify_credential_provider_unregister(changed)

    def test_complete_inventory_and_dll_signature(self):
        source = (ROOT / 'crates/oink-winsvc/src/platform/update/msi.rs').read_text()
        self.assertIn('"OinkCredentialProvider.dll"', source)
        self.assertIn('ends_with(".dll")', source)
