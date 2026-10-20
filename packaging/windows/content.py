"""Inspect decompiled MSI contents without installation or custom-action execution."""
import json
from pathlib import Path
import xml.etree.ElementTree as ET
import hashlib
from report_manifest import image_identity
from provenance import verify_no_builder_paths
from identity import windows_product_version
from versioning import release_version


def elements(root, name):
    return [node for node in root.iter() if node.tag.rsplit('}', 1)[-1] == name]


def verify_install_ui(tables):
    source = Path(__file__).resolve().parent
    binary = dict(tables.get('Binary', []))
    for name, asset in [('WixUI_Bmp_Dialog', 'installer-background.bmp'), ('WixUI_Bmp_Banner', 'installer-banner.bmp')]:
        if binary.get(name) != hashlib.sha256((source / asset).read_bytes()).hexdigest():
            raise RuntimeError('MSI dialog/banner must embed the reviewed artwork instead of stock or stale images')
    properties = dict(tables.get('Property', []))
    icons = dict(tables.get('Icon', []))
    expected = hashlib.sha256((Path(__file__).resolve().parents[2] /
                               'apps/oink/src-tauri/icons/icon.ico').read_bytes()).hexdigest()
    if properties.get('ARPPRODUCTICON') != 'OinkIcon.ico' or icons.get('OinkIcon.ico') != expected:
        raise RuntimeError('MSI ARP icon must embed the existing Oink app ICO')
    shortcuts = {row[0]: row for row in tables.get('Shortcut', [])}
    for name, directory, component, target in [('StartMenuEntry', 'ProgramMenuFolder', 'UserEntry', 'Product'),
                                                 ('DesktopShortcut', 'DesktopFolder', 'DesktopEntry', '[#UiExe]')]:
        row = shortcuts.get(name)
        if not row or (row[1], row[3], row[4], row[8], row[9], row[11]) != (
                directory, component, target, 'OinkIcon.ico', '0', 'INSTALLFOLDER'):
            raise RuntimeError('MSI Start menu and desktop shortcuts must own the Oink icon and target')
    desktop = next((row for row in tables.get('Component', []) if row[0] == 'DesktopEntry'), None)
    if not desktop or desktop[2] != 'DesktopFolder' or not int(desktop[3]) & 64 or desktop[4] != 'OINK_DESKTOP_SHORTCUT = "1"':
        raise RuntimeError('MSI desktop shortcut must be a transitive conditional machine component')
    if ('OINK_DESKTOP_SHORTCUT' not in properties.get('SecureCustomProperties', '').split(';') or
            'OINK_DESKTOP_SHORTCUT' in properties):
        raise RuntimeError('MSI desktop choice must transfer securely without a quiet-install default')
    remembered = [row for row in tables.get('Registry', []) if row[3] == 'DesktopShortcut']
    searches = [row for row in tables.get('RegLocator', []) if row[3] == 'DesktopShortcut']
    if (len(remembered) != 1 or remembered[0][1] != '2' or remembered[0][4:] != ['[OINK_DESKTOP_SHORTCUT]', 'InstallRoot'] or
            len(searches) != 1 or searches[0][1] != '2' or not int(searches[0][4]) & 16 or
            ['OINK_SAVED_DESKTOP', searches[0][0]] not in tables.get('AppSearch', [])):
        raise RuntimeError('MSI desktop choice must be remembered machine-wide for upgrades and repair')
    controls = {(row[0], row[1]): row for row in tables.get('Control', [])}
    dialogs = {row[0]: row for row in tables.get('Dialog', [])}
    for dialog, following in [('OinkWelcomeDlg', 'LicenseText'), ('OinkExitDlg', 'LaunchChoice')]:
        artwork = controls.get((dialog, 'Artwork'))
        if (dialog not in dialogs or dialogs[dialog][7] != 'Artwork' or not artwork or
                artwork[2] != 'Bitmap' or artwork[10] != following):
            raise RuntimeError('MSI artwork must lead the control chain so it cannot repaint over native controls')
    license_text = controls.get(('OinkWelcomeDlg', 'LicenseText'))
    if (not license_text or license_text[2] != 'ScrollableText' or int(license_text[5]) < 200 or
            int(license_text[6]) < 130 or int(license_text[7]) & 3 != 3 or
            not license_text[9].startswith('{\\rtf1') or 'GNU GENERAL PUBLIC LICENSE' not in license_text[9] or
            'Additional terms under GNU GPL' not in license_text[9]):
        raise RuntimeError('MSI license must be complete visible RTF in a readable scrollable viewport')
    for control, property_name, label in [('LaunchChoice', 'OinkLaunch', 'Launch Oink'),
                                           ('LoginChoice', 'OinkLogin', 'Start Oink when I sign in')]:
        row = controls.get(('OinkExitDlg', control))
        if not row or row[2] != 'CheckBox' or row[8:10] != [property_name, label] or properties.get(property_name) != '1':
            raise RuntimeError('MSI finish choices must both default checked')
        if ['OinkExitDlg', control, 'Show', 'UILevel = 5 AND NOT Installed AND NOT REMOVE'] not in tables.get('ControlCondition', []):
            raise RuntimeError('MSI finish choices must only appear after successful full-UI install/upgrade')
    desktop_control = controls.get(('OinkWelcomeDlg', 'DesktopChoice'))
    if not desktop_control or desktop_control[8:10] != ['OinkDesktop', 'Create a desktop shortcut']:
        raise RuntimeError('MSI desktop choice belongs on the welcome/license page')
    checks = dict(tables.get('CheckBox', []))
    if any(checks.get(name) != '1' for name in ('OinkDesktop', 'OinkLaunch', 'OinkLogin')):
        raise RuntimeError('MSI checkbox checked values must be the explicit value one')
    ui_sequence = {row[0]: int(row[2]) for row in tables.get('InstallUISequence', [])}
    order = ['AppSearch', 'DefaultDesktopChoice', 'ShowDesktopChoice', 'OinkWelcomeDlg', 'CostFinalize', 'ExecuteAction']
    if any(name not in ui_sequence for name in order) or any(
            ui_sequence[a] >= ui_sequence[b] for a, b in zip(order, order[1:])):
        raise RuntimeError('MSI desktop choice must be finalized before conditional component costing')
    actions = {row[0]: row for row in tables.get('CustomAction', [])}
    if actions.get('FinishInstall', [])[1:4] != ['1', 'InstallerCA', 'FinishInstall']:
        raise RuntimeError('MSI finish action must be immediate, impersonating client DLL code')
    for table in ('InstallExecuteSequence', 'InstallUISequence'):
        if any(row[0] == 'FinishInstall' for row in tables.get(table, [])):
            raise RuntimeError('MSI finish action must never be scheduled by an install sequence')
    event = ['OinkExitDlg', 'Finish', 'DoAction', 'FinishInstall',
             'UILevel = 5 AND NOT Installed AND NOT REMOVE AND (OinkLaunch = "1" OR OinkLogin = "1")', '1']
    if event not in tables.get('ControlEvent', []):
        raise RuntimeError('MSI finish action must be gated by the full-UI Finish button')
    events = tables.get('ControlEvent', [])
    for expected in [['OinkExitDlg', 'Finish', '[OinkFinishFailed]', '[OinkFinishFailed]', '1', '2'],
                     ['OinkExitDlg', 'Finish', 'SpawnDialog', 'OinkFinishWarningDlg', 'OinkFinishFailed = "1"', '3'],
                     ['OinkExitDlg', 'Finish', 'EndDialog', 'Return', '1', '999'],
                     ['OinkFinishWarningDlg', 'Done', 'EndDialog', 'Return', '1', '1']]:
        if expected not in events:
            raise RuntimeError('MSI Finish must refresh its result and show the owned warning before closing')
    warning = controls.get(('OinkFinishWarningDlg', 'Description'))
    if not warning or 'Open Oink from the Start menu' not in warning[9]:
        raise RuntimeError('MSI Finish failure dialog must explain how to launch and check login choices')
    default = next((row for row in tables.get('InstallUISequence', []) if row[0] == 'DefaultDesktopChoice'), None)
    if (actions.get('DefaultDesktopChoice', [])[1:4] != ['51', 'OINK_DESKTOP_SHORTCUT', '1'] or
            not default or default[1] != 'UILevel = 5 AND NOT Installed AND NOT WIX_UPGRADE_DETECTED AND NOT OINK_DESKTOP_SHORTCUT' or
            any(row[0] == 'DefaultDesktopChoice' for row in tables.get('InstallExecuteSequence', []))):
        raise RuntimeError('MSI desktop default must be full-UI fresh-install only')

    verify_user_session_ui(tables)


def verify_user_session_ui(tables):
    actions = {row[0]: row for row in tables.get('CustomAction', [])}
    for name, kind in [('PrepareUserSessions', '257'), ('RefreshUserSessions', '1')]:
        if actions.get(name, [])[1:4] != [kind, 'InstallerCA', name]:
            raise RuntimeError('MSI graceful stop must be immediate impersonating DLL code, once per installation')
    properties = dict(tables.get('Property', []))
    if 'OINK_GRACEFUL_STOP' in properties or 'OINK_GRACEFUL_STOP' not in properties.get('SecureCustomProperties', '').split(';'):
        raise RuntimeError('MSI quiet graceful stop must be a secure explicit opt-in without a default')
    ui = {row[0]: row for row in tables.get('InstallUISequence', [])}
    execute = {row[0]: row for row in tables.get('InstallExecuteSequence', [])}
    if (ui.get('PrepareUserSessions', ['', ''])[1] != 'UILevel = 5 AND NOT UPGRADINGPRODUCTCODE' or
            ui.get('OinkRunningDlg', ['', ''])[1] != 'OinkSessionsRunning = "1"' or
            not int(ui['PrepareUserSessions'][2]) < int(ui['OinkRunningDlg'][2]) < int(ui['ExecuteAction'][2]) or
            execute.get('PrepareUserSessions', ['', ''])[1] != 'UILevel < 5 AND NOT UPGRADINGPRODUCTCODE' or
            not int(execute['PrepareUserSessions'][2]) < int(execute['CheckInstallPath'][2]) < int(execute['InstallInitialize'][2]) or
            'RefreshUserSessions' in ui or 'RefreshUserSessions' in execute):
        raise RuntimeError('MSI running-session gate must precede elevation/transaction and exclude nested upgrade removal')
    controls = {(row[0], row[1]): row for row in tables.get('Control', [])}
    heading = controls.get(('OinkRunningDlg', 'Heading'))
    if not heading or heading[2] != 'Text' or 'Oink is running. Quit Oink and click Retry.' not in heading[9]:
        raise RuntimeError('MSI running-session dialog must explain how to continue')
    events = tables.get('ControlEvent', [])
    for expected in [['OinkRunningDlg', 'Retry', 'DoAction', 'RefreshUserSessions', '1', '1'],
                     ['OinkRunningDlg', 'Retry', 'EndDialog', 'Return', 'NOT OinkSessionsRunning', '2'],
                     ['OinkRunningDlg', 'Cancel', 'EndDialog', 'Exit', '1', '1']]:
        if expected not in events:
            raise RuntimeError('MSI running-session Retry must recheck without closing early, Cancel must abort')


def verify_registration(tables, metadata, identity):
    if metadata['version'] != release_version(identity['version'], metadata.get('source_commit')):
        raise RuntimeError('MSI display version differs from the source commit')
    properties = dict(tables.get('Property', []))
    numeric = windows_product_version(metadata['build'])
    if properties.get('ProductVersion') != numeric:
        raise RuntimeError('MSI numeric upgrade version differs from its release build')
    key = r'Software\Microsoft\Windows\CurrentVersion\Uninstall\[ProductCode]'
    display = [row for row in tables.get('Registry', []) if row[2] == key and row[3] == 'DisplayVersion']
    if (len(display) != 1 or display[0][1:] !=
            ['2', key, 'DisplayVersion', metadata['version'], 'InstallRoot']):
        raise RuntimeError('MSI ARP display version must contain the human release version')
    root = [row for row in tables.get('Component', []) if row[0] == 'InstallRoot']
    if len(root) != 1 or root[0][2] != 'INSTALLFOLDER' or not int(root[0][3]) & 256:
        raise RuntimeError('MSI ARP display version must use the machine-wide 64-bit component')
    sequence = {row[0]: int(row[2]) for row in tables.get('InstallExecuteSequence', [])}
    ordered = ['InstallInitialize', 'RemoveRegistryValues', 'RegisterProduct',
               'WriteRegistryValues', 'InstallExecute', 'RemoveExistingProducts', 'StartServices', 'InstallFinalize']
    if any(name not in sequence for name in ordered) or any(
            sequence[left] >= sequence[right] for left, right in zip(ordered, ordered[1:])) or any(
            row[1] for row in tables.get('InstallExecuteSequence', [])
            if row[0] in ('RegisterProduct', 'WriteRegistryValues')):
        raise RuntimeError('MSI registration sequence would overwrite ARP or escape the upgrade transaction')
    guards = ['InstallInitialize', 'CheckProtectedState', 'StopServices', 'CheckQuiescence',
              'DeleteServices', 'RemoveFiles']
    if any(name not in sequence for name in guards) or any(
            sequence[left] >= sequence[right] for left, right in zip(guards, guards[1:])) or any(
            row[1] for row in tables.get('InstallExecuteSequence', [])
            if row[0] in ('CheckProtectedState', 'CheckQuiescence')):
        raise RuntimeError('MSI quiescence guards must cover upgrade, uninstall and repair around StopServices')
    cleanup = [row for row in tables.get('CustomAction', []) if row[0] == 'CleanupMaintenance']
    maintenance = [row for row in tables.get('Component', []) if row[0] == 'MaintenanceRoot']
    conditions = {row[0]: row[1] for row in tables.get('InstallExecuteSequence', [])}
    if (len(cleanup) != 1 or int(cleanup[0][1]) != 3585 or cleanup[0][2:4] != ['InstallerCA', 'CleanupMaintenance'] or
            len(maintenance) != 1 or int(maintenance[0][3]) & 16 or
            conditions.get('CleanupMaintenance') != 'REMOVE~="ALL" AND NOT UPGRADINGPRODUCTCODE' or
            not sequence.get('StartServices', 0) < sequence.get('CleanupMaintenance', 0) < sequence['InstallFinalize']):
        raise RuntimeError('MSI must clean maintenance only on committed standalone full uninstall, after service restart')
    cloud = [row for row in tables.get('CustomAction', []) if row[0] == 'CleanupCloudFiles']
    if (len(cloud) != 1 or int(cloud[0][1]) != 3073 or
            cloud[0][2:4] != ['InstallerCA', 'CleanupCloudFiles'] or
            conditions.get('CleanupCloudFiles') != 'REMOVE~="ALL" AND NOT UPGRADINGPRODUCTCODE' or
            not sequence['CheckQuiescence'] < sequence.get('CleanupCloudFiles', 0) < sequence['DeleteServices']):
        raise RuntimeError('MSI must unregister per-user Cloud Files only on full uninstall, after quiescence and before file removal')
    upgrade = identity['windows']['upgrade_code'].upper()
    ranges = {row[6]: row for row in tables.get('Upgrade', [])}
    older, newer = ranges.get('WIX_UPGRADE_DETECTED'), ranges.get('WIX_DOWNGRADE_DETECTED')
    if (not older or not newer or any(row[0].strip('{}').upper() != upgrade for row in (older, newer)) or
            older[1] or older[2] != numeric or int(older[4]) & (2 | 512) or
            newer[1] != numeric or newer[2] or not int(newer[4]) & 2):
        raise RuntimeError('MSI major upgrade ranges must compare the numeric release build')


def verify_msi(manifest, extracted, identity, tables):
    root = ET.parse(manifest).getroot()
    files = elements(root, 'File')
    names = {node.get('Name'): node for node in files}
    expected = {identity['binaries'][role] + '.exe' for role in
                ('ui', 'agent', 'service', 'update_worker', 'crash_collector')}
    actual = {name for name in names if name and name.lower().endswith('.exe')}
    if actual != expected:
        raise RuntimeError('MSI executable inventory differs from the five product roles')
    components = []
    for name in sorted(expected):
        path = extracted / 'File' / names[name].get('Id')
        if not path.is_file() or path.read_bytes()[:2] != b'MZ':
            raise RuntimeError('MSI payload executable is missing or invalid')
        image_identity(path)  # Require actual x64 CodeView identity for every role.
        verify_no_builder_paths(path)
        components.append({'path': name, 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()})
    service = elements(root, 'ServiceInstall')
    if len(service) != 1 or service[0].get('Name') != identity['windows']['service_name'] or service[0].get('Account') != 'LocalSystem' or service[0].get('Start') != 'auto':
        raise RuntimeError('MSI must install one automatic LocalSystem OinkInput service')
    rules = elements(root, 'FirewallException')
    if len(rules) != 2 or {rule.get('Protocol') for rule in rules} != {'udp', 'tcp'}:
        raise RuntimeError('MSI must contain exactly the UDP and TCP firewall rules')
    ui_id = names[identity['binaries']['ui'] + '.exe'].get('Id')
    for rule in rules:
        # WiX's decompiler emits the resolved formatted Program field, rather
        # than the authoring-time File shortcut.
        if (rule.get('Program') != f'[#{ui_id}]' or rule.get('Profile') != 'all' or
                rule.get('Scope') != 'any' or rule.get('Outbound', 'no') != 'no'):
            raise RuntimeError('MSI firewall rules must target oink.exe on all profiles')
        if rule.get('Protocol') == 'tcp' and (rule.get('Name') != 'Oink - Bulk transfer (TCP)' or
                rule.get('Description') != 'Oink - Bulk transfer (TCP)'):
            raise RuntimeError('MSI TCP rule is not the bulk-transfer rule')
    permissions = tables.get('MsiLockPermissionsEx', [])
    folders = tables.get('CreateFolder', [])
    expected = {
        ('INSTALLFOLDER', 'CreateFolder', 'O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)', ''),
        ('MAINTENANCEFOLDER', 'CreateFolder', 'O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)', ''),
    }
    if (len(permissions) != 2 or {tuple(row[1:]) for row in permissions} != expected or
            len(folders) != 2 or {tuple(row) for row in folders} !=
            {('INSTALLFOLDER', 'InstallRoot'), ('MAINTENANCEFOLDER', 'MaintenanceRoot')}):
        raise RuntimeError('MSI protected installation/maintenance ACLs missing')
    if not any('WEBVIEW2RUNTIME' in node.get('Condition', '') for node in elements(root, 'Launch')):
        raise RuntimeError('MSI WebView2 prerequisite check missing')
    for name in (identity['windows']['receipt'], identity['metadata_file']):
        if name not in names:
            raise RuntimeError('MSI build metadata missing')
        record = json.loads((extracted / 'File' / names[name].get('Id')).read_text())
        if len(record['source_tree_sha256']) != 64:
            raise RuntimeError('MSI build provenance mismatch')
    if 'report-manifest.json' not in names:
        raise RuntimeError('MSI crash report manifest missing')
    report = json.loads((extracted / 'File' / names['report-manifest.json'].get('Id')).read_text())
    ui = extracted / 'File' / ui_id
    cli = extracted / 'File' / names[identity['binaries']['agent'] + '.exe'].get('Id')
    expected = [{'slot': slot, **image_identity(image)} for slot, image in
                [('agent', ui), ('agent', cli), ('ui_rust', ui)]]
    if report['source_tree_sha256'] != record['source_tree_sha256'] or report['images'] != expected:
        raise RuntimeError('MSI crash report manifest differs from shipped images')
    verify_registration(tables, record, identity)
    verify_install_ui(tables)
    return components
