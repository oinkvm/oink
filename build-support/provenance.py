"""Bind packaging to the exact binaries emitted by the recorded build."""
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess

ROOT = Path(__file__).resolve().parents[1]
EXCLUDED = {'.git', 'target', 'node_modules', 'dist', 'build', 'evidence',
            '.release-tools', '__pycache__', '.DS_Store', '.claude', 'AGENTS.md', 'CLAUDE.md'}
PRODUCT_DOCUMENTS = {'README.md', 'README.zh-CN.md', 'README.ja.md', 'README.ko.md',
                     'LICENSE-ADDITIONAL-TERMS.md', 'PRIVACY.md', 'THIRD_PARTY_LICENSES.md'}


def product_source(relative):
    """The same input files are built, accepted, and exported to a clean repo."""
    path = Path(relative)
    if any(part in EXCLUDED for part in path.parts):
        return False
    if relative.startswith('apps/oink/src-tauri/gen/'):
        # Tauri emits IDE permission schemas during compilation. Archives have
        # no Git ignore engine, so exclude this generated directory explicitly.
        return False
    if path.parts[0] == 'docs':
        return False
    if path.suffix.lower() == '.md' and path.parts[0] != 'licenses':
        return relative in PRODUCT_DOCUMENTS
    return True


def source_inventory(root=ROOT):
    """Git supplies portable file modes, never the source revision or file bytes.

    The hash covers working-tree bytes, including uncommitted source changes.
    Source archives may supply this inventory separately for mode preservation.
    """
    root = Path(root)
    modes = {}
    paths = None
    if (root / '.git').exists():
        index = subprocess.check_output(['git', 'ls-files', '--stage', '-z'], cwd=root)
        for entry in index.split(b'\0'):
            if entry:
                meta, path = entry.split(b'\t', 1)
                modes[os.fsdecode(path)] = meta.split()[0].decode()
        paths = [os.fsdecode(path) for path in subprocess.check_output(
            ['git', 'ls-files', '-co', '--exclude-standard', '-z'], cwd=root).split(b'\0') if path]
    if paths is None:
        paths = []
        for directory, dirs, files in os.walk(root):
            dirs[:] = sorted(d for d in dirs if d not in EXCLUDED)
            paths.extend((Path(directory) / name).relative_to(root).as_posix()
                         for name in files + [d for d in dirs if (Path(directory) / d).is_symlink()])
    entries = []
    for relative in sorted(set(paths)):
        path = root / relative
        if not product_source(relative) or not (path.exists() or path.is_symlink()):
            continue
        mode = '120000' if path.is_symlink() else '100755' if path.stat().st_mode & stat.S_IXUSR else '100644'
        if os.name == 'nt':
            mode = modes.get(relative, mode)
        data = os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
        entries.append({'path': relative, 'mode': mode, 'size': len(data),
                        'sha256': hashlib.sha256(data).hexdigest()})
    return sorted(entries, key=lambda item: item['path'].encode('utf-8'))


def source_tree_sha256(root=ROOT, inventory=None):
    """SHA256 of sorted UTF-8 path NUL mode NUL length NUL raw bytes.

    Length is ASCII decimal, making the stream unambiguous even for binary data.
    Symlinks contribute their link text with mode 120000, never target bytes.
    """
    result = hashlib.sha256()
    entries = source_inventory(root) if inventory is None else inventory
    if inventory is not None and {entry['path'] for entry in source_inventory(root)} != {entry['path'] for entry in entries}:
        raise RuntimeError('Source file inventory changed')
    if len({entry['path'] for entry in entries}) != len(entries):
        raise ValueError('duplicate source inventory path')
    for entry in sorted(entries, key=lambda item: item['path'].encode('utf-8')):
        relative = entry['path']
        path = Path(root) / relative
        if Path(relative).is_absolute() or '..' in Path(relative).parts or entry['mode'] not in ('100644', '100755', '120000'):
            raise ValueError('source inventory contains an invalid path')
        if os.name != 'nt':
            actual_mode = '120000' if path.is_symlink() else '100755' if path.stat().st_mode & stat.S_IXUSR else '100644'
            if actual_mode != entry['mode']:
                raise RuntimeError(f'Source mode changed: {relative}')
        data = os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
        if len(data) != entry['size'] or hashlib.sha256(data).hexdigest() != entry['sha256']:
            raise RuntimeError(f'Source changed: {relative}')
        result.update(relative.encode('utf-8') + b'\0' + entry['mode'].encode('ascii') + b'\0')
        result.update(str(len(data)).encode('ascii') + b'\0' + data)
    return result.hexdigest()


def digest(path):
    with path.open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()


def verify_no_builder_paths(path, directories=None):
    """Check runtime strings too: stripping debug records does not remove __FILE__."""
    if directories is None:
        directories = (ROOT, os.environ.get('CARGO_HOME') or Path.home() / '.cargo', Path.home())
    data = Path(path).read_bytes().lower()
    for directory in directories:
        text = str(directory)
        for spelling in {text, text.replace('\\', '/'), text.replace('/', '\\')}:
            for encoding in ('utf-8', 'utf-16le'):
                if spelling.encode(encoding).lower() in data:
                    raise RuntimeError('Builder filesystem path embedded in product executable: ' + Path(path).name)


def verify(directory, source_tree, version, target, build, source_commit=None):
    record = json.loads((directory / 'release-build.json').read_text())
    if (type(build) is not int or not 1 <= build < 2**31 or
            type(record.get('build')) is not int or record['build'] != build):
        raise RuntimeError('Binary release build does not match packaging request; rebuild with the requested --build')
    if (record['source_tree_sha256'], record['version'], record['target']) != (source_tree, version, target):
        raise RuntimeError('Binary build provenance does not match packaging request')
    if record.get('source_commit') != source_commit:
        raise RuntimeError('Binary source commit does not match packaging request')
    if record['identity_sha256'] != digest(ROOT / 'packaging/identity.json'):
        raise RuntimeError('Release identity changed after binary build')
    for name, expected in record['files'].items():
        if digest(directory / name) != expected:
            raise RuntimeError('Binary changed after build')
    return record


if __name__ == '__main__':
    import argparse
    parser = argparse.ArgumentParser(description='Hash the source tree; optionally write a portable file inventory outside it.')
    parser.add_argument('--inventory', type=Path)
    args = parser.parse_args()
    files = source_inventory()
    tree = source_tree_sha256(ROOT, files)
    if args.inventory:
        if args.inventory.resolve().is_relative_to(ROOT):
            parser.error('write the source inventory outside the source tree')
        args.inventory.parent.mkdir(parents=True, exist_ok=True)
        args.inventory.write_text(json.dumps({'schema': 1, 'source_tree_sha256': tree, 'files': files}, indent=2) + '\n')
    print(tree)
