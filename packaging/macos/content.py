"""Read-only checks of the assembled Mac product before disk-image creation."""
import json
import plistlib
import re
import subprocess
from signing import private_run, verify_signature
from provenance import digest, verify_no_builder_paths
from report_manifest import image_identity
from localizations import verify_localizations
from macos_imports import verify_imports
from versioning import release_version, macos_version_fields


def verify_microphone(app, source_tree_sha256, team_id):
    plugin = app / 'Contents/Resources/AudioPlugin/OinkMicrophone.driver'
    plugin_info = plistlib.loads((plugin / 'Contents/Info.plist').read_bytes())
    if (plugin_info.get('CFBundleIdentifier') != 'com.hachiwii.oink.microphone.driver'
            or plugin_info.get('CFBundleExecutable') != 'OinkMicrophone'
            or plugin_info.get('OinkGitSHA') != source_tree_sha256
            or not re.fullmatch(r'[a-f0-9]{64}', plugin_info.get('OinkPluginVersion', ''))):
        raise RuntimeError('Virtual microphone identity/provenance mismatch')
    verify_signature(plugin, team_id)
    if not (app / 'Contents/Resources/AudioPlugin/install.sh').is_file():
        raise RuntimeError('Virtual microphone installer missing')


def verify_tcc_policy(info, entitlements):
    # AppKit's Apple Event registration and WebKit's media capability preflights
    # do not require permission to automate other apps or capture the camera.
    # Oink uses neither feature; never grant access just to silence those checks.
    for entitlement, description in (
            ('com.apple.security.automation.apple-events', 'NSAppleEventsUsageDescription'),
            ('com.apple.security.device.camera', 'NSCameraUsageDescription')):
        if entitlements.get(entitlement) or description in info:
            raise RuntimeError(f'Unused TCC permission in Mac package: {entitlement}')


def verify_update_transport(info):
    # Local-network exceptions belong only in external qualification exports.
    # Never ship a production app capable of relaxing URLSession's TLS policy.
    if 'NSAppTransportSecurity' in info:
        raise RuntimeError('ATS exceptions are forbidden in production Mac packages')
    # No Sparkle runs in the app. Only its public key stays, for <=0.3.5 clients.
    if any(key.startswith('SU') and key != 'SUPublicEDKey' for key in info):
        raise RuntimeError('Sparkle configuration in production Mac package')


def verify_audio_capture_host(info, entitlements, backend):
    # Grant executes in the responsible host, not its nested UI helper. Check
    # the purpose string and hardened-runtime audio entitlement on that bundle.
    if info.get('CFBundleExecutable') != backend:
        raise RuntimeError('System audio request must belong to the backend main bundle')
    purpose = info.get('NSAudioCaptureUsageDescription')
    if not isinstance(purpose, str) or not purpose.strip():
        raise RuntimeError('Backend NSAudioCaptureUsageDescription is required')
    if entitlements.get('com.apple.security.device.audio-input') is not True:
        raise RuntimeError('Backend hardened runtime requires the audio-input entitlement')


def verify_product_name(bundle, info, name):
    if any(info.get(key) != name for key in ('CFBundleName', 'CFBundleDisplayName')):
        raise RuntimeError('Mac user-visible bundle names must be ' + name)
    # A differing filesystem name overrides localized display names on macOS.
    if bundle.name != name + '.app':
        raise RuntimeError('Mac app basename must match its user-visible name: ' + name)


def verify_component_groups(role, entitlements, files_group):
    if 'keychain-access-groups' in entitlements:
        raise RuntimeError('Unexpected restricted Keychain entitlement')
    expected = [files_group] if role in ('backend', 'file_provider_manager', 'file_provider_extension', 'finder_sync', 'paste_helper') else []
    if entitlements.get('com.apple.security.application-groups', []) != expected:
        raise RuntimeError('Component App Group isolation mismatch: ' + role)


def verify_manager_signing_identity(details, backend_id):
    expected = 'Identifier=' + backend_id + '.file-provider-manager'
    if expected not in details.splitlines():
        raise RuntimeError('File Provider manager must not share backend Keychain signing identity')


def verify_app(app, identity, architecture='arm64'):
    verify_imports([app], identity['macos_minimum'])
    if any(app.rglob('*.dSYM')) or any(app.rglob('*.pdb')) or any(app.rglob('*.map')):
        raise RuntimeError('Private symbols must not be embedded in the Mac app')
    b, mac = identity['binaries'], identity['macos']
    info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
    verify_product_name(app, info, identity['name'])
    group = mac['team_id'] + '.' + mac['app_group_suffix']
    if (info['CFBundleIdentifier'], info['CFBundleExecutable'], info['LSMinimumSystemVersion'],
            info[mac['app_group_key']]) != (identity['bundle_id'], b['backend'], '14.2', group):
        raise RuntimeError('Mac host identity, deployment target or App Group mismatch')
    helper = app / 'Contents/Helpers/Oink.app'
    ui = plistlib.loads((helper / 'Contents/Info.plist').read_bytes())
    verify_product_name(helper, ui, identity['name'])
    if ui['CFBundleIdentifier'] != mac['ui_id'] or ui['LSMinimumSystemVersion'] != '14.2':
        raise RuntimeError('Mac UI bundle identity or deployment target mismatch')
    extension = app / 'Contents/PlugIns' / mac['file_provider_bundle']
    ext_info = plistlib.loads((extension / 'Contents/Info.plist').read_bytes())
    if ext_info[mac['app_group_key']] != group or ext_info['LSMinimumSystemVersion'] != '14.2':
        raise RuntimeError('File Provider identity or deployment target mismatch')
    finder = app / 'Contents/PlugIns/OinkFinderSync.appex'
    finder_info = plistlib.loads((finder / 'Contents/Info.plist').read_bytes())
    paste = app / 'Contents/Helpers/OinkPaste.app'
    paste_info = plistlib.loads((paste / 'Contents/Info.plist').read_bytes())
    if (finder_info['CFBundleIdentifier'] != identity['bundle_id'] + '.findersync'
            or finder_info[mac['app_group_key']] != group
            or finder_info['NSExtension']['NSExtensionPointIdentifier'] != 'com.apple.FinderSync'
            or paste_info['CFBundleIdentifier'] != identity['bundle_id'] + '.paste'
            or paste_info['CFBundleExecutable'] != 'OinkPaste'
            or paste_info[mac['app_group_key']] != group):
        raise RuntimeError('Paste as File shell integration identity mismatch')
    binaries = {role: app / 'Contents/MacOS' / b[role] for role in
                ('backend', 'agent', 'crash_collector', 'file_provider_manager')}
    binaries.update(ui=helper / 'Contents/MacOS' / b['ui'],
                    file_provider_extension=extension / 'Contents/MacOS' / b['file_provider_extension'],
                    finder_sync=finder / 'Contents/MacOS/OinkFinderSync',
                    paste_helper=paste / 'Contents/MacOS/OinkPaste')
    components = []
    for role, binary in binaries.items():
        signed = private_run('codesign', '-d', '--entitlements', ':-', binary)
        entitlements = plistlib.loads(signed.stdout.encode()) if signed.stdout.strip() else {}
        verify_component_groups(role, entitlements, group)
        if role == 'file_provider_manager':
            details = private_run('codesign', '-dv', binary).stderr
            verify_manager_signing_identity(details, identity['bundle_id'])
        # Private dSYM/debug-map records have already been moved out and stripped.
        verify_no_builder_paths(binary)
        arches = subprocess.check_output(['lipo', '-archs', str(binary)], text=True).strip()
        commands = subprocess.check_output(['otool', '-l', str(binary)], text=True)
        minimum = re.findall(r'^\s+minos\s+(\S+)', commands, re.MULTILINE)
        minimum += re.findall(r'cmd LC_VERSION_MIN_MACOSX\n.*?version\s+(\S+)', commands, re.DOTALL)
        if arches != architecture or minimum not in (['14.2'], ['14.2.0']):
            raise RuntimeError(f'Expected {architecture} and macOS 14.2 deployment target: {binary.name}')
        components.append({'role': role, 'path': binary.relative_to(app).as_posix(), 'sha256': digest(binary),
                           'image_identity': image_identity(binary),
                           'architecture': architecture, 'minimum_os': '14.2', 'signature_verified': True})
    for bundle, bundle_info in ((app, info), (helper, ui), (extension, ext_info), (finder, finder_info), (paste, paste_info)):
        verify_update_transport(bundle_info)
        verify_localizations(bundle, bundle_info, permissions=bundle == app)
        result = private_run('codesign', '-d', '--entitlements', ':-', bundle)
        entitlements = plistlib.loads(result.stdout.encode()) if result.stdout.strip() else {}
        verify_tcc_policy(bundle_info, entitlements)
        if bundle == app:
            verify_audio_capture_host(bundle_info, entitlements, b['backend'])
        role = 'backend' if bundle == app else 'ui' if bundle == helper else 'finder_sync' if bundle == finder else 'paste_helper' if bundle == paste else 'file_provider_extension'
        verify_component_groups(role, entitlements, group)
    metadata = json.loads((app / 'Contents/Resources' / identity['metadata_file']).read_text())
    from provenance import ROOT
    trust = json.loads((ROOT / 'packaging/update-trust.json').read_bytes())['mac']
    expected_policy = 'feed-v2' if trust['enabled'] else 'manual-only'
    if (metadata['update_policy'] != expected_policy or len(metadata['source_tree_sha256']) != 64 or
            info.get('SUPublicEDKey') != trust['public_key'] or
            (trust['enabled'] and not trust['public_key'])):
        raise RuntimeError('Mac package provenance or update trust mismatch')
    if metadata['version'] != release_version(identity['version'], metadata.get('source_commit'), metadata['build']):
        raise RuntimeError('Mac display version differs from the release build')
    for bundle_info in (info, ui, ext_info, finder_info, paste_info):
        if any(bundle_info.get(key) != value for key, value in macos_version_fields(metadata['version'], metadata['build']).items()):
            raise RuntimeError('Mac numeric/display bundle versions differ from the release receipt')
    report = json.loads((app / 'Contents/Resources/report-manifest.json').read_text())
    # The runtime manifest has four image slots. Additional signed components
    # belong to the role-keyed package inventory above, not to its UI/FP slots.
    expected = [('agent', binaries['backend']), ('agent', binaries['agent']),
                ('ui_rust', binaries['ui']), ('file_provider', binaries['file_provider_extension'])]
    if (report['schema'] != 1 or report['source_tree_sha256'] != metadata['source_tree_sha256'] or
            report['images'] != [{'slot': slot, **image_identity(image)} for slot, image in expected] or
            (helper / 'Contents/Resources/report-manifest.json').read_bytes() !=
            (app / 'Contents/Resources/report-manifest.json').read_bytes()):
        raise RuntimeError('Mac crash report manifest differs from shipped images')
    if (app / 'Contents/Frameworks').exists():
        raise RuntimeError('Mac app must not embed frameworks (Sparkle was removed)')
    verify_microphone(app, metadata['source_tree_sha256'], mac['team_id'])
    verify_signature(app, mac['team_id'])
    return components
