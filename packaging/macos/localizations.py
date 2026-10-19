"""Bundle language declarations and OS-owned permission prompt resources."""
import plistlib

DECLARATIONS = {'CFBundleDevelopmentRegion': 'en', 'CFBundleLocalizations': ['en', 'zh-Hans', 'ja', 'ko']}
STRINGS = {
    'en': {
        'CFBundleName': 'Oink',
        'CFBundleDisplayName': 'Oink',
        'NSAudioCaptureUsageDescription': 'Oink shares system audio with your selected devices.',
        'NSLocalNetworkUsageDescription': 'Oink discovers and connects your devices on the local network.',
    },
    'zh-Hans': {
        'CFBundleName': 'Oink',
        'CFBundleDisplayName': 'Oink',
        'NSAudioCaptureUsageDescription': 'Oink 与你选择的设备共享系统音频。',
        'NSLocalNetworkUsageDescription': 'Oink 在局域网中发现并连接你的设备。',
    },
    'ja': {
        'CFBundleName': 'Oink',
        'CFBundleDisplayName': 'Oink',
        'NSAudioCaptureUsageDescription': 'Oink は、選択したデバイスとシステムオーディオを共有します。',
        'NSLocalNetworkUsageDescription': 'Oink は、ローカルネットワーク上のデバイスを検出して接続します。',
    },
    'ko': {
        'CFBundleName': 'Oink',
        'CFBundleDisplayName': 'Oink',
        'NSAudioCaptureUsageDescription': 'Oink는 선택한 기기와 시스템 오디오를 공유합니다.',
        'NSLocalNetworkUsageDescription': 'Oink는 로컬 네트워크에서 기기를 찾아 연결합니다.',
    },
}


def localized_strings(language, permissions):
    return {key: value for key, value in STRINGS[language].items()
            if permissions or not key.endswith('UsageDescription')}


def write_localizations(bundle, permissions=False):
    # Both the backend host and the nested WKWebView helper must advertise the
    # same languages. Otherwise macOS can select English for the helper even
    # though the user's first preferred language is one the UI supports.
    for language in STRINGS:
        directory = bundle / 'Contents/Resources' / (language + '.lproj')
        directory.mkdir(parents=True, exist_ok=True)
        (directory / 'InfoPlist.strings').write_bytes(plistlib.dumps(localized_strings(language, permissions)))


def verify_localizations(bundle, info, permissions=False):
    if any(info.get(key) != value for key, value in DECLARATIONS.items()):
        raise RuntimeError('Mac bundle must declare the en, zh-Hans, ja and ko localizations')
    for language in STRINGS:
        resource = bundle / 'Contents/Resources' / (language + '.lproj') / 'InfoPlist.strings'
        if not resource.is_file() or plistlib.loads(resource.read_bytes()) != localized_strings(language, permissions):
            raise RuntimeError('Mac localized InfoPlist.strings missing or incorrect: ' + language)
    if permissions and any(info.get(key) != value for key, value in STRINGS['en'].items()):
        raise RuntimeError('Mac permission fallback strings differ from English localization')
