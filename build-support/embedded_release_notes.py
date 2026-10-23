"""Embed approved release copy as data; no runtime updater behavior changes."""
import hashlib
import json
from pathlib import Path
import subprocess
from update.feed import loads, validate_notes


def write(root, output, build, source_commit):
    root = Path(root)
    path = root / '.github/update-notes.json'
    raw = path.read_bytes()
    if (root / '.git').exists():
        approved = subprocess.check_output(['git', '-C', str(root), 'show', 'HEAD:.github/update-notes.json'])
        if raw != approved:
            raise ValueError('embedded release notes must match the committed approval')
    annotations = loads(raw)
    if (set(annotations) != {'channel', 'severity', 'notes'} or annotations['channel'] != 'stable' or
            annotations['severity'] not in ('normal', 'critical')):
        raise ValueError('invalid approved release annotations')
    notes = validate_notes(annotations['notes'])
    if not {'en', 'zh-Hans', 'ja', 'ko'}.issubset(notes):
        raise ValueError('release packages require approved en/zh-Hans/ja/ko notes')
    # Match the existing package resource format, without importing the newer
    # local-installer feature or its in-house Git-history generation.
    value = {'schema': 1, 'kind': 'inhouse' if source_commit else 'public',
             'build': build, 'source_commit': source_commit, 'previous': None, 'notes': notes}
    data = (json.dumps(value, ensure_ascii=False, sort_keys=True, indent=2) + '\n').encode('utf-8')
    if len(data) > 128 * 1024:
        raise ValueError('embedded release notes exceed resource limit')
    output = Path(output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(data)
    return output


def verify_windows(tables, notes_file):
    expected = hashlib.sha256(Path(notes_file).read_bytes()).hexdigest()
    rows = [row for row in tables.get('Binary', []) if row[0] == 'OinkLocalUpdateNotes']
    if rows != [['OinkLocalUpdateNotes', expected]]:
        raise RuntimeError('MSI embedded notes differ from the committed approved copy')
