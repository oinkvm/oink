import importlib.util
import copy
import json
import hashlib
import struct
from pathlib import Path
import tempfile
import unittest
import uuid
import xml.etree.ElementTree as ET
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support'))
from native_image_fixtures import pe_image
from report_manifest import write_manifest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('windows_content', ROOT / 'packaging/windows/content.py')
C = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(C)
IDENTITY = {**json.loads((ROOT / 'packaging/identity.json').read_text()), 'version': '0.1.0'}  # Historical receipt fixture.
BUILDER_SPEC = importlib.util.spec_from_file_location('windows_packager', ROOT / 'packaging/windows/build.py')
B = importlib.util.module_from_spec(BUILDER_SPEC)
BUILDER_SPEC.loader.exec_module(B)


class MsiContent(unittest.TestCase):
    def test_numeric_upgrade_order_is_independent_of_beta_display_versions(self):
        builds = [1, 2, 3, 65535, 65536, 16777215, 16777216, 2147483647]
        versions = [tuple(map(int, B.windows_product_version(build).split('.'))) for build in builds]
        self.assertEqual(versions, sorted(set(versions)))
        self.assertEqual(B.windows_product_version(1), '0.0.1')
        self.assertEqual(B.windows_product_version(2), '0.0.2')
        for build in (0, -1, 2147483648, True, '2'):
            with self.assertRaises(ValueError):
                B.windows_product_version(build)

    def test_receipt_contract_excludes_detached_release_fields(self):
        fixture = json.loads((ROOT / 'packaging/tests/fixtures/windows-receipt.json').read_text())
        record = {**fixture, 'provenance': 'source-tree', 'signing': 'unsigned'}
        self.assertEqual(B.installed_receipt(record), fixture)

    def fixture(self, root):
        xml = ET.Element('Wix')
        extracted = root / 'extracted'
        (extracted / 'File').mkdir(parents=True)
        for role in ('ui', 'agent', 'service', 'update_worker', 'crash_collector'):
            ET.SubElement(xml, 'File', Name=IDENTITY['binaries'][role] + '.exe', Id=role)
            guid = uuid.UUID('11223344-5566-7788-99aa-bbccddeeff00') if role == 'agent' else None
            (extracted / 'File' / role).write_bytes(pe_image(guid) if guid else pe_image())
        for name in ('installed.json', 'oink-build.json'):
            ET.SubElement(xml, 'File', Name=name, Id=name)
            (extracted / 'File' / name).write_text(json.dumps({
                'source_tree_sha256': 'a' * 64,
                'version': '0.1.0', 'build': 1}))
        ET.SubElement(xml, 'File', Name='report-manifest.json', Id='report')
        write_manifest(extracted / 'File/report', 'a' * 64, '0.1.0', 1,
                       [('agent', extracted / 'File/ui'), ('agent', extracted / 'File/agent'),
                        ('ui_rust', extracted / 'File/ui')])
        ET.SubElement(xml, 'ServiceInstall', Name='OinkInput', Account='LocalSystem', Start='auto')
        for protocol in ('udp', 'tcp'):
            ET.SubElement(xml, 'FirewallException', Protocol=protocol, Program='[#ui]', Profile='all', Scope='any',
                          Name='Oink - Bulk transfer (TCP)', Description='Oink - Bulk transfer (TCP)')
        acl = {'MsiLockPermissionsEx': [
            ['root-acl', 'INSTALLFOLDER', 'CreateFolder', 'O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)', ''],
            ['maintenance-acl', 'MAINTENANCEFOLDER', 'CreateFolder', 'O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)', '']],
            'CreateFolder': [['INSTALLFOLDER', 'InstallRoot'], ['MAINTENANCEFOLDER', 'MaintenanceRoot']]}
        arp_key = r'Software\Microsoft\Windows\CurrentVersion\Uninstall\[ProductCode]'
        upgrade = '{' + IDENTITY['windows']['upgrade_code'] + '}'
        acl.update({
            'Property': [['ProductVersion', '0.0.1']],
            'Registry': [['ArpDisplayVersion', '2', arp_key, 'DisplayVersion', '0.1.0', 'InstallRoot']],
            'Component': [['InstallRoot', '', 'INSTALLFOLDER', '260', '', 'InstallRootKey'],
                          ['MaintenanceRoot', '', 'MAINTENANCEFOLDER', '256', '', '']],
            'CustomAction': [['CleanupMaintenance', '3649', 'InstallerCA', 'CleanupMaintenance'],
                             ['CleanupUserData', '3649', 'InstallerCA', 'CleanupUserData']],
            'InstallExecuteSequence': [[action, '', str(sequence)] for action, sequence in [
                ('InstallInitialize', 1500), ('RemoveRegistryValues', 2600), ('RegisterProduct', 6100),
                ('WriteRegistryValues', 6101), ('InstallExecute', 6500),
                ('RemoveExistingProducts', 6501), ('InstallFinalize', 6600), ('StartServices', 6502),
                ('CheckProtectedState', 1502), ('StopServices', 1900), ('CheckQuiescence', 1901),
                ('CostFinalize', 1000), ('InstallValidate', 1400), ('SetARPINSTALLLOCATION', 1001), ('DeleteServices', 2000), ('RemoveFiles', 3500)]],
            'Upgrade': [[upgrade, '', '0.0.1', '', '1', '', 'WIX_UPGRADE_DETECTED'],
                        [upgrade, '0.0.1', '', '', '2', '', 'WIX_DOWNGRADE_DETECTED']],
        })
        acl['InstallExecuteSequence'].append(['CleanupMaintenance', 'REMOVE~="ALL" AND NOT UPGRADINGPRODUCTCODE', '6599'])
        acl['InstallExecuteSequence'].append(['CleanupUserData', 'REMOVE~="ALL" AND NOT UPGRADINGPRODUCTCODE', '6598'])
        acl['Property'] += [['ARPPRODUCTICON', 'OinkIcon.ico'], ['OinkLaunch', '1'], ['OinkLogin', '1'],
                            ['SecureCustomProperties', 'OINK_DESKTOP_SHORTCUT'], ['ALLUSERS', '1'], ['MSIRESTARTMANAGERCONTROL', 'Disable']]
        acl['Icon'] = [['OinkIcon.ico', hashlib.sha256((ROOT / 'apps/oink/src-tauri/icons/icon.ico').read_bytes()).hexdigest()]]
        acl['Binary'] = [[name, hashlib.sha256((ROOT / 'packaging/windows' / asset).read_bytes()).hexdigest()]
                         for name, asset in [('WixUI_Bmp_Dialog', 'installer-background.bmp'), ('WixUI_Bmp_Banner', 'installer-banner.bmp')]]
        acl['Dialog'] = [[name, '50', '50', '370', '270', '3', 'Oink Setup', 'Artwork', default, default]
                         for name, default in [('OinkWelcomeDlg', 'Install'), ('OinkExitDlg', 'Finish')]]
        acl['Shortcut'] = [[name, directory, 'Oink', component, target, '', '', '', 'OinkIcon.ico', '0', '', 'INSTALLFOLDER']
                           for name, directory, component, target in [
                               ('StartMenuEntry', 'ProgramMenuFolder', 'UserEntry', '[#UiExe]'),
                               ('DesktopShortcut', 'DesktopFolder', 'DesktopEntry', '[#UiExe]')]]
        acl['MsiShortcutProperty'] = [[name + 'Identity', name, 'System.AppUserModel.ID', IDENTITY['bundle_id'] + '.ui']
                                      for name in ('StartMenuEntry', 'DesktopShortcut')]
        acl['CustomAction'].append(['SetARPINSTALLLOCATION', '51', 'ARPINSTALLLOCATION', '[INSTALLFOLDER]'])
        acl['Component'] += [['DesktopEntry', '', 'DesktopFolder', '324', 'OINK_DESKTOP_SHORTCUT = "1"', 'DesktopKey']]
        acl['Registry'] += [['DesktopChoice', '2', 'Software\\Oink', 'DesktopShortcut', '[OINK_DESKTOP_SHORTCUT]', 'InstallRoot']]
        acl['RegLocator'] = [['SavedDesktopChoice', '2', 'Software\\Oink', 'DesktopShortcut', '18']]
        acl['AppSearch'] = [['OINK_SAVED_DESKTOP', 'SavedDesktopChoice']]
        acl['Control'] = [[dialog, control, 'CheckBox', '0', '0', '200', '18', '3', prop, label]
                          for dialog, control, prop, label in [
                              ('OinkExitDlg', 'LaunchChoice', 'OinkLaunch', 'Launch Oink'),
                              ('OinkExitDlg', 'LoginChoice', 'OinkLogin', 'Start Oink when I sign in'),
                              ('OinkWelcomeDlg', 'DesktopChoice', 'OinkDesktop', 'Create a desktop shortcut')]]
        license_rtf = root / 'license.rtf'
        B.write_license_rtf(license_rtf)
        acl['Control'] += [['OinkWelcomeDlg', 'LicenseText', 'ScrollableText', '130', '36', '226', '142', '7', '', license_rtf.read_text(), 'DesktopChoice'],
                           ['OinkWelcomeDlg', 'Artwork', 'Bitmap', '0', '0', '370', '234', '1', '', 'WixUI_Bmp_Dialog', 'LicenseText'],
                           ['OinkExitDlg', 'Artwork', 'Bitmap', '0', '0', '370', '234', '1', '', 'WixUI_Bmp_Dialog', 'LaunchChoice']]
        acl['ControlCondition'] = [['OinkExitDlg', control, 'Show', 'UILevel = 5 AND NOT Installed AND NOT REMOVE']
                                   for control in ['LaunchChoice', 'LoginChoice']]
        acl['ControlEvent'] = [['OinkExitDlg', 'Finish', 'DoAction', 'FinishInstall',
                               'UILevel = 5 AND NOT Installed AND NOT REMOVE AND (OinkLaunch = "1" OR OinkLogin = "1")', '1']]
        acl['Control'] += [['OinkFinishWarningDlg', 'Description', 'Text', '20', '49', '330', '58', '3', '',
                            'Open Oink from the Start menu and check Start at login in Settings.']]
        acl['ControlEvent'] += [['OinkExitDlg', 'Finish', '[OinkFinishFailed]', '[OinkFinishFailed]', '1', '2'],
                                ['OinkExitDlg', 'Finish', 'SpawnDialog', 'OinkFinishWarningDlg', 'OinkFinishFailed = "1"', '3'],
                                ['OinkExitDlg', 'Finish', 'EndDialog', 'Return', '1', '999'],
                                ['OinkFinishWarningDlg', 'Done', 'EndDialog', 'Return', '1', '1']]
        acl['CheckBox'] = [[name, '1'] for name in ('OinkDesktop', 'OinkLaunch', 'OinkLogin')]
        acl['CustomAction'] += [['FinishInstall', '1', 'InstallerCA', 'FinishInstall'],
                                ['DefaultDesktopChoice', '51', 'OINK_DESKTOP_SHORTCUT', '1']]
        acl['InstallUISequence'] = [['DefaultDesktopChoice',
                                    'UILevel = 5 AND NOT Installed AND NOT WIX_UPGRADE_DETECTED AND NOT OINK_DESKTOP_SHORTCUT', '997'],
                                   ['AppSearch', '', '400'], ['ShowDesktopChoice', '', '998'],
                                   ['OinkWelcomeDlg', 'NOT Installed', '999'], ['CostFinalize', '', '1000'],
                                   ['ExecuteAction', '', '1300']]
        acl['CustomAction'].append(['PrepareUserSessions', '257', 'InstallerCA', 'PrepareUserSessions'])
        acl['InstallUISequence'].append(['PrepareUserSessions', 'UILevel = 5 AND NOT UPGRADINGPRODUCTCODE', '1298'])
        acl['InstallExecuteSequence'] += [['PrepareUserSessions', 'NOT UPGRADINGPRODUCTCODE', '1399'],
                                          ['CheckInstallPath', '', '1499']]
        ET.SubElement(xml, 'Launch', Condition='Installed OR WEBVIEW2RUNTIME')
        return xml, extracted, acl

    def test_start_menu_maps_to_removable_machine_product(self):
        with tempfile.TemporaryDirectory() as temp:
            _, _, tables = self.fixture(Path(temp))
            C.verify_start_registration(tables, IDENTITY)
            for table, name, column, value in [
                    ('CustomAction', 'SetARPINSTALLLOCATION', 3, 'INSTALLFOLDER'),
                    ('InstallExecuteSequence', 'SetARPINSTALLLOCATION', 2, '999'),
                    ('InstallExecuteSequence', 'SetARPINSTALLLOCATION', 1, 'NOT Installed'),
                    ('Shortcut', 'StartMenuEntry', 4, 'Product'),
                    ('MsiShortcutProperty', 'StartMenuEntryIdentity', 3, 'unstable.id'),
                    ('Property', 'ALLUSERS', 1, '2')]:
                changed = copy.deepcopy(tables)
                next(row for row in changed[table] if row[0] == name)[column] = value
                with self.subTest(table=table), self.assertRaisesRegex(RuntimeError, 'Start menu registration'):
                    C.verify_start_registration(changed, IDENTITY)
            for flag in ('ARPNOREMOVE', 'ARPSYSTEMCOMPONENT'):
                changed = copy.deepcopy(tables)
                changed['Property'].append([flag, '1'])
                with self.assertRaisesRegex(RuntimeError, 'Start menu registration'):
                    C.verify_start_registration(changed, IDENTITY)

    def test_real_uninstall_deletes_data_but_upgrade_does_not(self):
        with tempfile.TemporaryDirectory() as temp:
            _, _, tables = self.fixture(Path(temp))
            C.verify_data_cleanup(tables)
            for table, column, value in [('CustomAction', 1, '3585'),
                                         ('CustomAction', 1, '3073'),
                                         ('InstallExecuteSequence', 1, 'REMOVE~="ALL"'),
                                         ('InstallExecuteSequence', 1, ''),
                                         ('InstallExecuteSequence', 2, '6601')]:
                changed = copy.deepcopy(tables)
                next(row for row in changed[table] if row[0] == 'CleanupUserData')[column] = value
                with self.subTest(table=table), self.assertRaisesRegex(RuntimeError, 'committed standalone uninstall'):
                    C.verify_data_cleanup(changed)

    def test_post_uninstall_cleanup_cannot_rollback_product_removal(self):
        source = (ROOT / 'packaging/windows/Package.wxs').read_text()
        actions = {node.get('Id'): node for node in ET.fromstring(source).iter()
                   if node.tag.rsplit('}', 1)[-1] == 'CustomAction'}
        for name in ('CleanupUserData', 'CleanupMaintenance'):
            with self.subTest(action=name):
                self.assertEqual(actions[name].get('Return'), 'ignore')

    def test_license_cannot_be_empty_hidden_or_painted_over(self):
        with tempfile.TemporaryDirectory() as temp:
            _, _, tables = self.fixture(Path(temp))
            C.verify_install_ui(tables)
            for table, name, column, value in [('Control', 'LicenseText', 9, ''), ('Control', 'LicenseText', 7, '4'),
                                              ('Control', 'LicenseText', 5, '20'), ('Control', 'Artwork', 10, ''),
                                              ('Dialog', 'OinkWelcomeDlg', 7, 'LicenseText'),
                                              ('Binary', 'WixUI_Bmp_Dialog', 1, '0' * 64),
                                              ('Binary', 'WixUI_Bmp_Banner', 1, '0' * 64)]:
                changed = copy.deepcopy(tables)
                row = next(row for row in changed[table] if row[1 if table == 'Control' else 0] == name)
                row[column] = value
                with self.subTest(table=table, name=name), self.assertRaises(RuntimeError):
                    C.verify_install_ui(changed)

    def test_running_session_quit_is_client_first_at_every_ui_level(self):
        with tempfile.TemporaryDirectory() as temp:
            _, _, tables = self.fixture(Path(temp))
            C.verify_user_session_ui(tables)
            for table, name, column, value in [
                    ('CustomAction', 'PrepareUserSessions', 1, '3073'),
                    ('InstallUISequence', 'PrepareUserSessions', 2, '1301'),
                    ('InstallExecuteSequence', 'PrepareUserSessions', 1, 'UILevel = 5'),
                    ('InstallExecuteSequence', 'PrepareUserSessions', 2, '1401'),
                    ('Property', 'MSIRESTARTMANAGERCONTROL', 1, '')]:
                changed = copy.deepcopy(tables)
                next(row for row in changed[table] if row[0] == name)[column] = value
                with self.subTest(table=table, name=name), self.assertRaises(RuntimeError):
                    C.verify_user_session_ui(changed)
            changed = copy.deepcopy(tables)
            changed['Property'].append(['OINK_GRACEFUL_STOP', '1'])
            with self.assertRaises(RuntimeError):
                C.verify_user_session_ui(changed)

    def test_finish_failure_is_a_conditional_child_dialog_before_setup_closes(self):
        with tempfile.TemporaryDirectory() as temp:
            _, _, tables = self.fixture(Path(temp))
            C.verify_install_ui(tables)
            for event in tables['ControlEvent'][1:5]:
                changed = copy.deepcopy(tables)
                changed['ControlEvent'].remove(event)
                with self.subTest(event=event), self.assertRaisesRegex(RuntimeError, 'warning before closing'):
                    C.verify_install_ui(changed)
            changed = copy.deepcopy(tables)
            next(row for row in changed['Control'] if row[:2] == ['OinkFinishWarningDlg', 'Description'])[9] = ''
            with self.assertRaisesRegex(RuntimeError, 'explain how to launch'):
                C.verify_install_ui(changed)

    def test_native_controls_have_plain_matching_bitmap_backgrounds(self):
        # Three-times vector rasterization remains sharp at 100/150/200% DPI.
        # The banner mark occupies only its rightmost reserved strip; the
        # welcome/finish controls all sit to the right of the logo gutter.
        for name, height, start_x, end_x in [
                ('installer-background.bmp', 312, 151, 493),
                ('installer-banner.bmp', 58, 0, 449)]:
            data = (ROOT / 'packaging/windows' / name).read_bytes()
            offset = struct.unpack_from('<I', data, 10)[0]
            width, signed_height, planes, bits = struct.unpack_from('<iiHH', data, 18)
            self.assertEqual((width, abs(signed_height), planes, bits), (1479, height * 3, 1, 24))
            stride = (width * 3 + 3) & ~3
            for y in range(height * 3):
                row = data[offset + y * stride + start_x * 9:offset + y * stride + end_x * 9]
                self.assertEqual(row, b'\xf0' * (end_x - start_x) * 9, (name, y))
            # Both images contain the existing dark cursor and pink B2 tail,
            # so a blank or old kite/animal asset cannot satisfy the layout.
            self.assertIn(bytes.fromhex('281e1b'), data[offset:])
            self.assertIn(bytes.fromhex('916fff'), data[offset:])

    def test_icons_and_finish_actions_cannot_escape_client_ui(self):
        with tempfile.TemporaryDirectory() as temp:
            _, _, tables = self.fixture(Path(temp))
            C.verify_install_ui(tables)
            for table, row, field, value in [
                    ('Icon', 0, 1, '0' * 64), ('Shortcut', 0, 8, ''), ('Shortcut', 1, 8, ''),
                    ('Shortcut', 1, 1, 'ProgramMenuFolder'), ('Control', 0, 9, 'Wrong'),
                    ('ControlCondition', 1, 3, '1'), ('ControlEvent', 0, 4, '1'),
                    ('CheckBox', 0, 1, '0'), ('InstallUISequence', 3, 2, '1299')]:
                changed = copy.deepcopy(tables)
                changed[table][row][field] = value
                with self.subTest(table=table, field=field), self.assertRaises(RuntimeError):
                    C.verify_install_ui(changed)
            for action_type in ('3073', '2049', '1025'):
                changed = copy.deepcopy(tables)
                next(row for row in changed['CustomAction'] if row[0] == 'FinishInstall')[1] = action_type
                with self.assertRaisesRegex(RuntimeError, 'client DLL'):
                    C.verify_install_ui(changed)
            for table in ('InstallExecuteSequence', 'InstallUISequence'):
                changed = copy.deepcopy(tables)
                changed[table].append(['FinishInstall', '1', '6700'])
                with self.assertRaisesRegex(RuntimeError, 'never be scheduled'):
                    C.verify_install_ui(changed)

    def test_desktop_is_remembered_conditional_and_has_no_quiet_default(self):
        with tempfile.TemporaryDirectory() as temp:
            _, _, tables = self.fixture(Path(temp))
            for table, predicate, field, value in [
                    ('Component', 'DesktopEntry', 4, ''), ('Component', 'DesktopEntry', 3, '260'),
                    ('Registry', 'DesktopChoice', 1, '1'), ('RegLocator', 'SavedDesktopChoice', 4, '2'),
                    ('InstallUISequence', 'DefaultDesktopChoice', 1, 'NOT Installed')]:
                changed = copy.deepcopy(tables)
                next(row for row in changed[table] if row[0] == predicate)[field] = value
                with self.subTest(table=table), self.assertRaises(RuntimeError):
                    C.verify_install_ui(changed)
            changed = copy.deepcopy(tables)
            changed['Property'].append(['OINK_DESKTOP_SHORTCUT', '1'])
            with self.assertRaisesRegex(RuntimeError, 'quiet-install default'):
                C.verify_install_ui(changed)

    def test_cleanup_is_commit_only_full_uninstall_after_service_restart(self):
        with tempfile.TemporaryDirectory() as temp:
            xml, extracted, tables = self.fixture(Path(temp))
            manifest = Path(temp) / 'decompiled.wxs'
            ET.ElementTree(xml).write(manifest)
            C.verify_msi(manifest, extracted, IDENTITY, tables)
            for action, value in [('CleanupMaintenance', ''), ('CleanupMaintenance', 'REMOVE~="ALL"'),
                                  ('CleanupMaintenance', 'NOT Installed')]:
                changed = copy.deepcopy(tables)
                next(row for row in changed['InstallExecuteSequence'] if row[0] == action)[1] = value
                with self.assertRaises(RuntimeError):
                    C.verify_msi(manifest, extracted, IDENTITY, changed)
            for table, row, field, value in [('CustomAction', 0, 1, '3073'), # Deferred, before commit.
                                             ('CustomAction', 0, 3, 'ReleaseMaintenance'),
                                             ('Component', 1, 3, '272')]: # Permanent directory.
                changed = copy.deepcopy(tables)
                changed[table][row][field] = value
                with self.assertRaises(RuntimeError):
                    C.verify_msi(manifest, extracted, IDENTITY, changed)
            changed = copy.deepcopy(tables)
            next(row for row in changed['InstallExecuteSequence'] if row[0] == 'CleanupMaintenance')[2] = '6501'
            with self.assertRaises(RuntimeError):
                C.verify_msi(manifest, extracted, IDENTITY, changed)

    def test_service_restart_and_unconditional_maintenance_guards(self):
        with tempfile.TemporaryDirectory() as temp:
            xml, extracted, tables = self.fixture(Path(temp))
            manifest = Path(temp) / 'decompiled.wxs'
            ET.ElementTree(xml).write(manifest)
            C.verify_msi(manifest, extracted, IDENTITY, tables)
            for action, field, value in [
                    ('StartServices', 2, '5900'), # Old MSI must see the broker stopped.
                    ('CheckProtectedState', 1, 'NOT Installed'),
                    ('CheckQuiescence', 1, 'NOT REMOVE'),
                    ('CheckProtectedState', 2, '1902'),
                    ('CheckQuiescence', 2, '1899')]:
                with self.subTest(action=action, value=value):
                    changed = copy.deepcopy(tables)
                    next(row for row in changed['InstallExecuteSequence'] if row[0] == action)[field] = value
                    with self.assertRaises(RuntimeError):
                        C.verify_msi(manifest, extracted, IDENTITY, changed)

    def test_arp_display_and_numeric_upgrade_registration_cannot_drift(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            xml, extracted, tables = self.fixture(root)
            manifest = root / 'decompiled.wxs'
            ET.ElementTree(xml).write(manifest)
            C.verify_msi(manifest, extracted, IDENTITY, tables)
            for table, row, field, value in [
                    ('Property', 0, 1, '0.1.0'),
                    ('Registry', 0, 1, '1'),
                    ('Registry', 0, 2, r'Software\Oink'),
                    ('Registry', 0, 4, '0.0.1'),
                    ('Registry', 0, 5, 'UnrelatedComponent'),
                    ('Component', 0, 3, '4'),
                    ('InstallExecuteSequence', 3, 1, 'NOT Installed'),
                    ('InstallExecuteSequence', 3, 2, '5000'),
                    ('InstallExecuteSequence', 3, 2, '6700'),
                    ('Upgrade', 0, 2, '0.0.2'),
                    ('Upgrade', 0, 4, '513'),
                    ('Upgrade', 1, 4, '0')]:
                with self.subTest(table=table, field=field, value=value):
                    changed = copy.deepcopy(tables)
                    changed[table][row][field] = value
                    with self.assertRaises(RuntimeError):
                        C.verify_msi(manifest, extracted, IDENTITY, changed)

    def test_commit_label_is_accepted_as_arp_and_receipt_version(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            xml, extracted, tables = self.fixture(root)
            display = '0.2.1-inhouse.0123abcd'
            for name in ('installed.json', 'oink-build.json'):
                path = extracted / 'File' / name
                receipt = json.loads(path.read_text())
                receipt.update(version=display)
                if name == 'oink-build.json':
                    receipt['source_commit'] = '0123abcd' * 5
                path.write_text(json.dumps(receipt))
            tables['Registry'][0][4] = display
            manifest = root / 'decompiled.wxs'
            ET.ElementTree(xml).write(manifest)
            C.verify_msi(manifest, extracted, IDENTITY, tables)
            tables['Registry'][0][4] = '0.1.0'
            with self.assertRaisesRegex(RuntimeError, 'human release version'):
                C.verify_msi(manifest, extracted, IDENTITY, tables)

    def test_payload_service_acl_prerequisite_and_firewall_contract(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            xml, extracted, acl = self.fixture(root)
            manifest = root / 'decompiled.wxs'
            ET.ElementTree(xml).write(manifest)
            C.verify_msi(manifest, extracted, IDENTITY, acl)
            for node, attribute, value in [
                    (xml.find('ServiceInstall'), 'Account', 'LocalService'),
                    (xml.find('FirewallException'), 'Program', '[#service]'),
                    (xml.find('FirewallException'), 'Profile', 'private')]:
                previous = node.get(attribute)
                node.set(attribute, value)
                ET.ElementTree(xml).write(manifest)
                with self.assertRaises(RuntimeError):
                    C.verify_msi(manifest, extracted, IDENTITY, acl)
                node.set(attribute, previous)

    def test_cli_crash_identity_cannot_be_omitted_or_replaced_by_ui(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            xml, extracted, acl = self.fixture(root)
            manifest = root / 'decompiled.wxs'
            ET.ElementTree(xml).write(manifest)
            report_path = extracted / 'File/report'
            original = json.loads(report_path.read_text())
            for images in (original['images'][:1] + original['images'][2:],
                           [original['images'][0], original['images'][0], original['images'][2]]):
                report_path.write_text(json.dumps({**original, 'images': images}))
                with self.assertRaisesRegex(RuntimeError, 'differs from shipped images'):
                    C.verify_msi(manifest, extracted, IDENTITY, acl)

    def test_acl_must_protect_the_actual_folders_unconditionally(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            xml, extracted, acl = self.fixture(root)
            manifest = root / 'decompiled.wxs'
            ET.ElementTree(xml).write(manifest)
            for table, row, field, value in [
                    ('MsiLockPermissionsEx', 0, 1, 'OTHERFOLDER'),
                    ('MsiLockPermissionsEx', 0, 2, 'File'),
                    ('MsiLockPermissionsEx', 0, 3, 'D:(A;;FA;;;WD)'),
                    ('MsiLockPermissionsEx', 1, 4, 'Installed'),
                    ('CreateFolder', 1, 1, 'UnrelatedComponent')]:
                changed = copy.deepcopy(acl)
                changed[table][row][field] = value
                with self.assertRaisesRegex(RuntimeError, 'protected installation/maintenance ACLs'):
                    C.verify_msi(manifest, extracted, IDENTITY, changed)
            with self.assertRaisesRegex(RuntimeError, 'protected installation/maintenance ACLs'):
                C.verify_msi(manifest, extracted, IDENTITY, {})

    def test_unexpected_executable_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            xml, extracted, acl = self.fixture(root)
            ET.SubElement(xml, 'File', Name='fixture-tool.exe', Id='tool')
            manifest = root / 'decompiled.wxs'
            ET.ElementTree(xml).write(manifest)
            with self.assertRaisesRegex(RuntimeError, 'five product roles'):
                C.verify_msi(manifest, extracted, IDENTITY, acl)
