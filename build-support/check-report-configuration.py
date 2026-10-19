#!/usr/bin/env python3
"""Check final package identities/report configuration without sending a report."""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from public_release import locate

ROOT = Path(__file__).resolve().parents[1]


def mac_status(app, record):
    backend = app / 'Contents/MacOS/oink-backend'
    env = dict(os.environ)
    for key in ['OINK_REPORT_DSN', 'OINK_REPORT_REGION', 'OINK_IPC_PORT']:
        env.pop(key, None)
    with tempfile.TemporaryDirectory(suffix='.noindex') as tmp, tempfile.TemporaryFile() as log:
        home = Path(tmp); env['OINK_HOME'] = tmp
        child = subprocess.Popen([str(backend), '--launch-smoke'], env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 15
            while not (home / 'launch-smoke-ready').exists():
                if child.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError('report configuration backend did not become ready')
                time.sleep(.05)
            endpoint = json.loads((home / 'ipc.json').read_text())
            with socket.create_connection(('127.0.0.1', endpoint['port']), timeout=5) as connection:
                connection.sendall((json.dumps({'token': endpoint['token'], 'build': {'version': record['version'], 'build': str(record['build']), 'source': record['source_tree_sha256']}, 'call': {'method': 'report_status'}}) + '\n').encode())
                status = json.loads(connection.makefile().readline())['ok']
            assert status['configured'] and status['consent']['choice'] == 'unasked'
            assert status['queued_crashes'] == status['queued_performance'] == 0
        finally:
            if child.poll() is None:
                child.terminate()
            try:
                assert child.wait(timeout=10) == 0
            except subprocess.TimeoutExpired:
                child.kill(); child.wait()
                raise RuntimeError('report configuration backend did not stop')


def windows_status(package, record):
    # Link the exact Cargo bin-oink dependencies, without starting the GUI,
    # backend, input hooks or installed service. Runtime has no DSN environment.
    cache = Path(os.environ['ProgramFiles(x86)']) / 'Microsoft Visual Studio/Installer/vswhere.exe'
    location = subprocess.check_output([str(cache), '-latest', '-products', '*', '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64', '-property', 'installationPath'], text=True).strip()
    devcmd = Path(location) / 'Common7/Tools/VsDevCmd.bat'
    for line in subprocess.check_output(f'cmd.exe /d /s /c ""{devcmd}" -arch=x64 >nul && set"', text=True).splitlines():
        key, sep, value = line.partition('=')
        if sep and key:
            os.environ[key] = value
    os.environ.pop('OINK_REPORT_DSN', None); os.environ.pop('OINK_REPORT_REGION', None)
    target = Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target')).resolve()
    release = target / 'x86_64-pc-windows-msvc/release'
    binary_fingerprints=list((release/'.fingerprint').glob('oink-ui-*/bin-oink.json'));assert len(binary_fingerprints)==1
    wanted=next(dep[3] for dep in json.loads(binary_fingerprints[0].read_text())['deps'] if dep[1]=='oink_agent')
    libraries=[]
    for candidate in (release/'deps').glob('*oink_agent-*.rlib'):
        key=candidate.stem.removeprefix('liboink_agent-')
        fingerprint=release/'.fingerprint'/('oink-agent-'+key)/'lib-oink_agent'
        if fingerprint.is_file() and int.from_bytes(bytes.fromhex(fingerprint.read_text().strip()),'little')==wanted:
            libraries.append(candidate)
    assert len(libraries)==1, 'UI/backend production library fingerprint did not match'
    ui_wanted=next(dep[3] for dep in json.loads(binary_fingerprints[0].read_text())['deps'] if dep[1]=='oink_ui')
    ui_libraries=[]
    for candidate in (release/'deps').glob('*oink_ui-*.rlib'):
        key=candidate.stem.removeprefix('liboink_ui-')
        fingerprint=release/'.fingerprint'/('oink-ui-'+key)/'lib-oink_ui'
        if fingerprint.is_file() and int.from_bytes(bytes.fromhex(fingerprint.read_text().strip()),'little')==ui_wanted:
            ui_libraries.append(candidate)
    assert len(ui_libraries)==1, 'UI production library fingerprint did not match'
    with tempfile.TemporaryDirectory(prefix='oink-report-check-') as temporary:
        directory=Path(temporary)
        executable=directory/'report-probe.exe';home=directory/'home';home.mkdir()
        shutil.copyfile(package/'payload/report-manifest.json',directory/'report-manifest.json')
        command=['rustc','--crate-name','oink_report_configuration_check','--edition=2024','--target','x86_64-pc-windows-msvc','-C','opt-level=3','-C','lto=thin','-C','codegen-units=1','-C','target-feature=+crt-static','-C','debuginfo=0','--extern','oink_agent='+str(libraries[0]),'--extern','oink_ui='+str(ui_libraries[0]),'-L','dependency='+str(release/'deps'),'-L','dependency='+str(target / 'release/deps')]
        for native in (release/'build').glob('*/out'):
            command+=['-L','native='+str(native)]
            for child in native.rglob('*'):
                if child.is_dir() and child.name.lower() in ('lib','release'):command+=['-L','native='+str(child)]
        # Reuse authoritative native paths emitted by the production Cargo builds.
        searches=set()
        for output in (release/'build').glob('*/output'):
            for line in output.read_text(encoding='utf-8',errors='replace').splitlines():
                for prefix in ('cargo:rustc-link-search=','cargo::rustc-link-search='):
                    if line.startswith(prefix):searches.add(line[len(prefix):])
        for search in sorted(searches):command+=['-L',search]
        command+=['-o',str(executable),str(ROOT / 'build-support/report-configuration-check.rs')]
        compiled=subprocess.run(command)
        if compiled.returncode:raise RuntimeError('Private production-library probe compilation failed')
        env=dict(os.environ,OINK_HOME=str(home),EXPECTED_VERSION=record['version'],EXPECTED_BUILD=str(record['build']))
        completed=subprocess.run([str(executable)],env=env,check=True,capture_output=True,text=True,encoding='utf-8',timeout=15)
        status=json.loads(completed.stdout);assert status['configured'] and not status['consent_granted'] and not status['report_sent']
        assert status['version_label']==env['EXPECTED_VERSION'] and status['ui_build']==str(record['build'])


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--packages', type=Path, required=True)
    p.add_argument('--platform', choices=['macos', 'windows'], required=True)
    a = p.parse_args(); path = locate(a.packages, a.platform); package = path.parent
    record = json.loads(path.read_text())
    private = os.environ.get('OINK_REPORT_DSN', '').encode()
    if not private:
        raise RuntimeError('reporting build configuration required')
    if a.platform == 'macos':
        sys.path.insert(0, str(ROOT / 'packaging/macos'))
        from content import verify_app
        from identity import IDENTITY
        from signing import private_run, ticket_operation
        from macos_imports import verify_imports
        from macos_registration import temporary_registration
        dmg = package / next(x['name'] for x in record['artifacts'] if x['name'].endswith('.dmg'))
        ticket_operation('validate', dmg)
        private_run('spctl', '--assess', '--type', 'open', '--context', 'context:primary-signature', dmg)
        archive = package / next(x['name'] for x in record['artifacts'] if x['name'].endswith('.zip'))
        with tempfile.TemporaryDirectory(suffix='.noindex') as tmp:
            subprocess.run(['ditto', '-x', '-k', str(archive), tmp], check=True)
            app = Path(tmp) / 'Oink.app'
            verify_app(app, IDENTITY); verify_imports([app], '14.2')
            ticket_operation('validate', app)
            private_run('spctl', '--assess', '--type', 'exec', app)
            assert all(private in (app / 'Contents/MacOS' / name).read_bytes() for name in ['oink-backend', 'oink-agent'])
            manager = (app / 'Contents/MacOS/oink-file-provider').read_bytes()
            assert all(c.encode() in manager for c in ['remove-domain', 'prepare-update', 'finish-update'])
            with temporary_registration(app, 'OinkFileProvider.appex', 'com.hachiwii.oink.fileprovider'):
                mac_status(app, record)
            cli = app / 'Contents/MacOS/oink-agent'
            with tempfile.TemporaryDirectory() as home:
                version = subprocess.check_output([str(cli), '--version'], env=dict(os.environ, OINK_HOME=home, OINK_IPC_PORT='0'), text=True)
                assert version.strip() == 'oink-agent ' + record['version']
        # Inspect the actual DMG payload without Finder or launching its host.
        with tempfile.TemporaryDirectory(suffix='.noindex') as tmp:
            mount = Path(tmp) / 'volume'; mount.mkdir()
            subprocess.run(['hdiutil', 'attach', '-readonly', '-nobrowse', '-noautoopen', '-mountpoint', str(mount), str(dmg)], check=True, capture_output=True)
            try:
                verify_app(mount / 'Oink.app', IDENTITY)
                verify_imports([mount / 'Oink.app'], IDENTITY['macos_minimum'])
                from provenance import digest
                assert digest(mount / 'Oink.app/Contents/MacOS/oink-backend') == record['launch_smoke']['backend_sha256']
            finally:
                subprocess.run(['hdiutil', 'detach', str(mount)], check=True, capture_output=True)
    else:
        assert private in (package / 'payload/oink.exe').read_bytes()
        with tempfile.TemporaryDirectory() as home:
            env = dict(os.environ, OINK_HOME=home, OINK_IPC_PORT='0')
            for arg in ['--version', '--help']:
                result = subprocess.check_output([str(package / 'payload/oink-agent.exe'), arg], env=env, text=True)
                assert record['version'] in result if arg == '--version' else 'oink' in result.lower()
            assert not list(Path(home).iterdir())
        windows_status(package, record)
    (package / 'report-configuration-check.json').write_text(json.dumps({'configured': True, 'report_sent': False, 'version': record['version'], 'build': record['build']}) + '\n')
    print('PASS: production version/reporting configuration, no report sent')


if __name__ == '__main__':
    main()
