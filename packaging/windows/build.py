#!/usr/bin/env python3
"""Build and decompile a per-machine MSI. Never invoke msiexec or install tools."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import sys
import re
import shutil
import subprocess
import urllib.request
import uuid
import zipfile
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support'))
from identity import signpath_configuration, windows_product_version
from provenance import verify, digest
from payload import copy_notices
from report_manifest import write_manifest, image_identity
import xml.etree.ElementTree as ET

from versioning import release_version

ROOT = Path(__file__).resolve().parents[2]
I = json.loads((ROOT / 'packaging/identity.json').read_text())
WIX_VERSION = '5.0.2'
WIX_SHA256 = 'f30ef0c74e2a986126539c5780be93ac24e8136eaf723b1937b26272703ae173'
UI_SHA256 = '5ef2c707614b9f70b6bbadd2d4abcb4124efee215e9b16bfbc80113079a604c7'
FIREWALL_SHA256 = 'c9a1c99afab82225127b34036b719dea6c98c451e21b341ae1c62a674d0ba5ef'


def run(*args, **kwargs):
    subprocess.run([str(a) for a in args], check=True, **kwargs)


def wix_tool(cache):
    """Unpack a hash-pinned CLI locally; no global tool/MSI installation."""
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / f'wix-{WIX_VERSION}.zip'
    if not archive.exists():
        urllib.request.urlretrieve(f'https://api.nuget.org/v3-flatcontainer/wix/{WIX_VERSION}/wix.{WIX_VERSION}.nupkg', archive)
    if digest(archive) != WIX_SHA256:
        raise RuntimeError('WiX download hash mismatch')
    destination = cache / f'wix-{WIX_VERSION}'
    with zipfile.ZipFile(archive) as source:
        source.extractall(destination)
    return ['dotnet', str(destination / 'tools/net6.0/any/wix.dll')]


def extension(cache, name, sha256):
    package = f'WixToolset.{name}.wixext'
    directory = cache / f'{package}-{WIX_VERSION}'
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / f'{package}-{WIX_VERSION}.zip'
    if not archive.exists():
        url = f'https://api.nuget.org/v3-flatcontainer/{package.lower()}/{WIX_VERSION}/{package.lower()}.{WIX_VERSION}.nupkg'
        urllib.request.urlretrieve(url, archive)
    if digest(archive) != sha256:
        raise RuntimeError(f'WiX {name} extension download hash mismatch')
    with zipfile.ZipFile(archive) as source:
        source.extractall(directory)
    return directory / 'wixext5' / f'{package}.dll'


def write_license_rtf(path):
    text = (ROOT / 'LICENSE').read_text() + '\n' + (ROOT / 'LICENSE-ADDITIONAL-TERMS.md').read_text()
    text = text.replace('\\', '\\\\').replace('{', '\\{').replace('}', '\\}').replace('\n', '\\par\n')
    encoded = ''.join(c if ord(c) < 128 else f'\\u{ord(c) if ord(c) < 32768 else ord(c)-65536}?' for c in text)
    path.write_text('{\\rtf1\\ansi{\\fonttbl{\\f0 Segoe UI;}}\\f0\\fs18 ' + encoded + '}', encoding='ascii')


def write_notices(path, payload, files):
    ns = 'http://wixtoolset.org/schemas/v4/wxs'
    ET.register_namespace('', ns)
    def node(parent, tag, **attrs):
        return ET.SubElement(parent, '{' + ns + '}' + tag, attrs)
    root = ET.Element('{' + ns + '}Wix')
    fragment = node(root, 'Fragment')
    directories = {'': node(fragment, 'DirectoryRef', Id='INSTALLFOLDER')}
    components = []
    for relative in files:
        current = ''
        for part in Path(relative).parts[:-1]:
            parent = current
            current = (Path(current) / part).as_posix()
            if current not in directories:
                identifier = 'NoticeDir_' + hashlib.sha256(current.encode()).hexdigest()[:20]
                directories[current] = node(directories[parent], 'Directory', Id=identifier, Name=part)
        identifier = 'Notice_' + hashlib.sha256(relative.encode()).hexdigest()[:20]
        component = node(directories[current], 'Component', Id=identifier, Guid='*')
        node(component, 'File', Id=identifier + '_file', Source=str(payload / relative), KeyPath='yes')
        components.append(identifier)
    group = node(fragment, 'ComponentGroup', Id='Notices')
    for identifier in components:
        node(group, 'ComponentRef', Id=identifier)
    ET.indent(root)
    path.write_bytes(ET.tostring(root, encoding='utf-8', xml_declaration=True))


def installed_receipt(record):
    # This is the strict service/worker receipt, not the richer detached release
    # record. Signing takes place after MSI creation, so verify the actual files.
    fields = ('schema', 'product', 'source_tree_sha256', 'version', 'build', 'architecture',
              'minimum_os', 'minimumWindowsBuild', 'updater_included', 'collector_included',
              'protocol_baseline', 'upgrade_code', 'components')
    return {**{key: record[key] for key in fields},
            'signature': 'verify Authenticode on final installed files; signing changes file hashes'}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binaries', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--tool-cache', type=Path, required=True)
    p.add_argument('--version', default=I['version'])
    p.add_argument('--source-commit', help='Full source commit for a local commit-labelled version')
    p.add_argument('--build', type=int, required=True)
    p.add_argument('--source-tree-sha256', required=True)
    p.add_argument('--require-updater', action='store_true')
    p.add_argument('--require-collector', action='store_true')
    a = p.parse_args()
    if not re.fullmatch(r'[0-9a-f]{64}', a.source_tree_sha256) or not 1 <= a.build < 2**31:
        p.error('source tree SHA256 and build in 1..2147483647 required')
    if a.version != I['version']:
        p.error('version must match packaging/identity.json')
    try:
        a.version = release_version(a.version, a.source_commit)
    except ValueError as error:
        p.error(str(error))
    provenance = verify(a.binaries, a.source_tree_sha256, a.version, 'x86_64-pc-windows-msvc', a.build, a.source_commit)
    has_worker = provenance.get('updater_included', False)
    if not has_worker or not provenance.get('collector_included'):
        p.error('the product MSI requires both the update worker and crash collector')
    has_collector = provenance.get('collector_included', False)
    if a.require_updater and not has_worker:
        p.error('the integrated update worker is required for a release package')
    if has_worker and not provenance.get('worker_static_crt', False):
        p.error('the copied update worker must be built with the static MSVC CRT')
    if a.require_collector and not has_collector:
        p.error('the integrated crash collector is required for a release package')
    a.output = a.output.resolve()
    a.output.mkdir(parents=True, exist_ok=True)
    payload = a.output / 'payload'
    payload.mkdir(exist_ok=True)
    b, w = I['binaries'], I['windows']
    roles = ['ui', 'agent', 'service'] + (['update_worker'] if has_worker else []) + (['crash_collector'] if has_collector else [])
    for role in roles:
        shutil.copy2(a.binaries / (b[role] + '.exe'), payload)
    report = write_manifest(payload / 'report-manifest.json', a.source_tree_sha256, a.version, a.build,
                            [('agent', payload / (b['ui'] + '.exe')),
                             ('agent', payload / (b['agent'] + '.exe')),
                             ('ui_rust', payload / (b['ui'] + '.exe'))])
    symbols = a.output / 'symbols.noindex'
    symbols.mkdir(exist_ok=True)
    symbol_records = []
    for role in roles:
        name = b[role]
        pdb = a.binaries / (name.replace('-', '_') + '.pdb')
        if not pdb.is_file():
            pdb = a.binaries / (name + '.pdb')
        if not pdb.is_file():
            raise RuntimeError('Private PDB missing for ' + name)
        shutil.copy2(pdb, symbols / pdb.name)
        symbol_records.append({'image': name + '.exe', **image_identity(payload / (name + '.exe')),
                               'pdb': pdb.name, 'pdb_sha256': digest(pdb)})
    (symbols / 'manifest.json').write_text(json.dumps({'source_tree_sha256': a.source_tree_sha256,
                                                     'images': symbol_records}, indent=2) + '\n')
    record = dict(schema=1, product=I['name'], provenance='source-tree', source_tree_sha256=a.source_tree_sha256, version=a.version, source_commit=a.source_commit, build=a.build,
                  architecture='x64', minimum_os=I['windows_minimum'], minimumWindowsBuild=I['minimumWindowsBuild'], protocol_baseline=1,
                  upgrade_code=w['upgrade_code'], components=[b[r] + '.exe' for r in roles], updater_included=has_worker,
                  collector_included=has_collector,
                  update_policy='manual-only', signing='unsigned', report_manifest=report)
    notice_files = copy_notices(payload)
    (payload / I['metadata_file']).write_text(json.dumps(record, indent=2) + '\n')
    (payload / w['receipt']).write_text(json.dumps(installed_receipt(record), indent=2) + '\n')
    # ProductVersion's MSI fields are 8/8/16 bits. Encode build, not display
    # semver: beta -> stable and display-version changes must never downgrade.
    msi_version = windows_product_version(a.build)
    product_code = str(uuid.uuid5(uuid.UUID(w['upgrade_code']), str(a.build))).upper()
    license_rtf = a.output / 'license.rtf'
    write_license_rtf(license_rtf)
    values = {'LicenseRtf': str(license_rtf),
              'DialogBmp': str(ROOT / 'packaging/windows/installer-background.bmp'),
              'BannerBmp': str(ROOT / 'packaging/windows/installer-banner.bmp'), 'ProductName': I['name'], 'Manufacturer': I['publisher'], 'ProductVersion': msi_version,
              'ProductCode': product_code, 'UpgradeCode': w['upgrade_code'],
              'MinimumWindowsBuild': str(I['minimumWindowsBuild']),
              'InstallDirectory': w['install_directory'], 'ServiceName': w['service_name'],
              'ServiceDisplayName': w['service_display_name'], 'RegistryKey': w['registry_key'],
              'MaintenanceDirectory': w['maintenance_directory'], 'UiExe': b['ui'] + '.exe',
              'AgentExe': b['agent'] + '.exe', 'ServiceExe': b['service'] + '.exe',
              'WorkerExe': b['update_worker'] + '.exe', 'HasUpdateWorker': '1' if has_worker else '0',
              'CollectorExe': b['crash_collector'] + '.exe', 'HasCrashCollector': '1' if has_collector else '0',
              'Receipt': w['receipt'], 'DisplayVersion': a.version, 'Build': str(a.build),
              'MaintenanceGuid': str(uuid.uuid5(uuid.UUID(w['upgrade_code']), 'maintenance-directory-v1')),
              'Payload': str(payload), 'CustomAction': str(a.output / 'InstallerCA.dll')}
    header = '#pragma once\n' + '\n'.join(f'#define ID_{key} L{json.dumps(value)}' for key, value in {
        'DATA_DIRECTORY': I['data_directory'], 'INSTALL_DIR': w['install_directory'], 'SERVICE': w['service_name'], 'UI': b['ui'] + '.exe',
        'AGENT': b['agent'] + '.exe', 'SERVICE_EXE': b['service'] + '.exe',
        'UPDATE_WORKER': b['update_worker'] + '.exe',
        'CRASH_COLLECTOR': b['crash_collector'] + '.exe',
        'MAINTENANCE': w['maintenance_directory'], 'RUN_VALUE': w['run_value'], 'RECEIPT': w['receipt'],
        'MAINTENANCE_LOCK': w['maintenance_lock'], 'COPIED_WORKER': w['copied_worker'],
        'INSTALLER_EVENT_PREFIX': w['installer_event_prefix'], 'PRODUCT_CODE': product_code,
        'UPGRADE_CODE': '{' + w['upgrade_code'] + '}'}.items()) + '\n'
    (a.output / 'identity.h').write_text(header)
    # Use existing MSVC build tools only. /MT avoids a VC runtime prerequisite.
    if not shutil.which('cl.exe'):
        vswhere = Path(os.environ['ProgramFiles(x86)']) / 'Microsoft Visual Studio/Installer/vswhere.exe'
        location = subprocess.check_output([str(vswhere), '-latest', '-products', '*', '-requires',
            'Microsoft.VisualStudio.Component.VC.Tools.x86.x64', '-property', 'installationPath'], text=True).strip()
        devcmd = Path(location) / 'Common7/Tools/VsDevCmd.bat'
        if not location or not devcmd.exists():
            raise RuntimeError('MSVC x64 build tools are required')
        environment = subprocess.check_output(f'cmd.exe /d /s /c ""{devcmd}" -arch=x64 >nul && set"', text=True)
        for line in environment.splitlines():
            key, separator, value = line.partition('=')
            if separator and key:
                os.environ[key] = value
    if has_worker:
        for role in roles:
            imports = subprocess.check_output(['dumpbin.exe', '/nologo', '/dependents', str(payload / (b[role] + '.exe'))], text=True)
            dependencies = re.findall(r'^\s*([A-Za-z0-9_.-]+\.dll)\s*$', imports, re.IGNORECASE | re.MULTILINE)
            # UCRT is an inbox Windows 10/11 component. No app binary may
            # need the VC redistributable;
            # the copied update worker additionally needs a fully static CRT.
            forbidden = r'(vcruntime|msvcp|msvcr|concrt)'
            if role == 'update_worker':
                forbidden = r'(vcruntime|msvcp|msvcr|concrt|ucrtbase|api-ms-win-crt)'
            if not dependencies or any(re.match(forbidden, name, re.IGNORECASE) for name in dependencies):
                raise RuntimeError('Unexpected external CRT dependency in ' + b[role])
            (a.output / (b[role] + '-dependencies.txt')).write_text(imports)
    run('cl.exe', '/nologo', '/LD', '/EHsc', '/std:c++17', '/W4', '/WX', '/MT',
        '/DUNICODE', '/D_UNICODE', f'/I{a.output}', ROOT / 'packaging/windows/InstallerCA.cpp',
        f'/Fo{a.output}/InstallerCA.obj', '/link', f'/OUT:{a.output}/InstallerCA.dll', f'/IMPLIB:{a.output}/InstallerCA.lib',
        'msi.lib', 'advapi32.lib', 'shell32.lib', 'ole32.lib', 'bcrypt.lib', 'user32.lib', 'userenv.lib')
    for test in ('AuthorizationTests', 'QuiescenceTests', 'MaintenanceCleanupTests', 'CloudFilesCleanupTests', 'FinishInstallTests', 'GracefulStopTests'):
        run('cl.exe', '/nologo', '/EHsc', '/std:c++17', '/W4', '/WX', '/MT', ROOT / f'packaging/windows/{test}.cpp',
            f'/Fo{a.output}/{test}.obj', f'/Fe{a.output}/{test}.exe', 'advapi32.lib', 'user32.lib')
        run(a.output / f'{test}.exe')
    notices = a.output / 'notices.wxs'
    write_notices(notices, payload, notice_files)
    run('node', ROOT / 'build-support/check-package-licenses.mjs', payload, 'windows-x64', cwd=ROOT)
    wix = wix_tool(a.tool_cache.resolve())
    firewall = extension(a.tool_cache.resolve(), 'Firewall', FIREWALL_SHA256)
    ui = extension(a.tool_cache.resolve(), 'UI', UI_SHA256)
    msi = a.output / f"{I['name']}-{a.version}-{a.build}-windows-x64.msi"
    defines = [part for key, value in values.items() for part in ['-d', f'{key}={value}']]
    defines += ['-d', 'AppIcon=' + str(ROOT / 'apps/oink/src-tauri/icons/icon.ico')]
    run(*wix, 'build', ROOT / 'packaging/windows/Package.wxs', ROOT / 'packaging/windows/Dialogs.wxs', notices, '-ext', firewall, '-ext', ui, '-arch', 'x64', '-culture', 'en-US', *defines,
        '-intermediateFolder', a.output / 'obj', '-o', msi)
    # Decompilation/extraction is read-only; never use msiexec /a for validation.
    run(*wix, 'msi', 'decompile', msi, '-ext', firewall, '-o', a.output / 'decompiled.wxs', '-x', a.output / 'extracted')
    from content import verify_msi
    from tables import read_content_tables
    tables = read_content_tables(msi)
    (a.output / 'content-tables.json').write_text(json.dumps(tables, indent=2) + '\n')
    record['embedded_executables'] = verify_msi(a.output / 'decompiled.wxs', a.output / 'extracted', I, tables)
    record.update(product_code=product_code, msi_version=msi_version, signing='unsigned',
                  artifacts=[dict(name=msi.name, size=msi.stat().st_size, sha256=hashlib.sha256(msi.read_bytes()).hexdigest())])
    (a.output / 'release.json').write_text(json.dumps(record, indent=2) + '\n')
    (a.output / 'SHA256SUMS').write_text(f'{digest(msi)}  {msi.name}\n')
    # The configuration is generated from the same identity as the MSI. Submit
    # only the MSI artifact, not the loose payload or intermediate build files.
    (a.output / 'signpath-artifact-configuration.xml').write_bytes(signpath_configuration())


if __name__ == '__main__':
    main()
