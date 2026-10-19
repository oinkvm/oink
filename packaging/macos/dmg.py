"""Create the Finder layout from prototype artwork without scripting Finder."""
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile


def verify_volume_icon(volume, icon):
    """Finder-free check of the custom-volume-icon bit and exact app artwork."""
    if (volume / '.VolumeIcon.icns').read_bytes() != icon.read_bytes():
        raise RuntimeError('DMG volume icon differs from the app icon')
    finder_info = bytes.fromhex(subprocess.check_output(
        ['xattr', '-px', 'com.apple.FinderInfo', str(volume)], text=True))
    if len(finder_info) != 32 or not int.from_bytes(finder_info[8:10], 'big') & 0x0400:
        raise RuntimeError('DMG volume is missing the custom icon Finder flag')


def verify_image_icon(image, icon):
    with tempfile.TemporaryDirectory(prefix='oink-dmg-icon-', suffix='.noindex') as temporary:
        mount = Path(temporary) / 'volume'
        result = subprocess.check_output(['hdiutil', 'attach', str(image), '-readonly', '-nobrowse',
                                          '-mountpoint', str(mount), '-plist'])
        mounted = plistlib.loads(result)
        try:
            # This audit reads only volume metadata. It never launches the app;
            # lsregister -u can reject an unregistered, read-only bundle. Smoke
            # cleanup belongs to the earlier writable staging launch instead.
            verify_volume_icon(mount, icon)
        finally:
            device = next(row['dev-entry'] for row in mounted['system-entities'] if row.get('mount-point'))
            subprocess.run(['hdiutil', 'detach', device], check=True, capture_output=True)


def create(app, output, cache):
    icon = app / 'Contents/Resources/icon.icns'
    tools = Path(cache) / 'dmgbuild-1.6.7'
    subprocess.run([sys.executable, '-m', 'pip', 'install', '--disable-pip-version-check',
                    '--require-hashes', '--only-binary=:all:', '--target', str(tools),
                    '-r', str(Path(__file__).with_name('requirements.txt'))], check=True)
    sys.path.insert(0, str(tools))
    import dmgbuild
    dmgbuild.build_dmg(str(output), 'Oink', settings={
        'format': 'UDZO', 'files': [str(app)], 'symlinks': {'Applications': '/Applications'},
        'icon': str(icon),
        'background': str(Path(__file__).with_name('dmg-background.png')),
        'window_rect': ((100, 100), (640, 392)),
        'icon_locations': {'Oink.app': (190, 144), 'Applications': (450, 144)},
        'icon_size': 104, 'text_size': 12, 'show_toolbar': False,
        'show_status_bar': False, 'show_pathbar': False, 'show_sidebar': False})
    verify_image_icon(output, icon)
