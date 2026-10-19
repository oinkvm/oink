"""Copy the same offline notices consumed by the product About window."""
import json
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parents[1]


def copy_notices(destination):
    manifest = json.loads((ROOT / 'third-party-licenses.json').read_text())
    paths = {'third-party-licenses.json', 'THIRD_PARTY_LICENSES.md', 'NOTICE', 'LICENSE',
             'LICENSE-ADDITIONAL-TERMS.md'}
    for component in manifest['components']:
        paths.update(license['path'] for license in component['licenseFiles'])
    for relative in sorted(paths):
        path = Path(relative)
        if path.is_absolute() or '..' in path.parts:
            raise ValueError('invalid attribution resource path')
        target = destination / path
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / path, target)
    return sorted(paths)
