"""Generate embedded package notes from approved copy or in-house Git history."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import unicodedata
from update.feed import validate_notes

MAX_FILE = 128 * 1024
MAX_TEXT = 16 * 1024
SHA = re.compile(r'[0-9a-f]{40}')
LANGUAGE = re.compile(r'[A-Za-z]{2,3}(?:-[A-Za-z0-9]{2,8})*')


def validate(value, build, source_commit):
    if (value.get('schema') != 1 or value.get('build') != build or
            value.get('source_commit') != source_commit or
            value.get('kind') != ('inhouse' if source_commit else 'public')):
        raise ValueError('embedded notes do not match this package')
    notes = value.get('notes')
    if not isinstance(notes, dict) or 'en' not in notes or not 1 <= len(notes) <= 32:
        raise ValueError('embedded notes need an English fallback')
    seen = set()
    for language, text in notes.items():
        if (not LANGUAGE.fullmatch(language) or language.lower() in seen or
                not isinstance(text, str) or not text.strip() or len(text.encode()) > MAX_TEXT or
                any(unicodedata.category(c) == 'Cc' and c not in '\n\t' for c in text)):
            raise ValueError('invalid embedded notes')
        seen.add(language.lower())
    return value


def read(path, build, source_commit, root=None):
    raw = Path(path).read_bytes()
    if len(raw) > MAX_FILE:
        raise ValueError('embedded notes file exceeds limit')
    value = validate(json.loads(raw), build, source_commit)
    if root is not None and not source_commit:
        approved = generate(root, build, None, Path(path).parent)
        if value != approved:
            raise ValueError('public embedded notes must match the committed approved notes')
    return value


def previous_record(output_root, build, explicit=None):
    paths = [Path(explicit)] if explicit else Path(output_root).rglob('release.json')
    records = []
    for path in paths:
        try:
            value = json.loads(path.read_bytes())
        except (OSError, ValueError):
            if explicit:
                raise ValueError('invalid previous in-house release record')
            continue
        number = value.get('build');commit = value.get('source_commit');label = value.get('version', '')
        if (type(number) is int and 0 < number < build and isinstance(commit, str) and
                SHA.fullmatch(commit) and isinstance(label, str) and label.endswith(('-inhouse.' + str(number), '-inhouse.' + commit[:8]))):
            records.append({'build':number, 'version':label, 'source_commit':commit})
        elif explicit:
            raise ValueError('previous in-house release must have a lower build and matching in-house label')
    if not records:
        return None
    number = max(record['build'] for record in records)
    latest = [record for record in records if record['build'] == number]
    if len({record['source_commit'] for record in latest}) != 1:
        raise ValueError('conflicting previous in-house release records')
    return latest[0]


def generate(root, build, source_commit, output_root, previous=None):
    root = Path(root)
    result = {'schema':1, 'kind':'inhouse' if source_commit else 'public',
              'build':build, 'source_commit':source_commit, 'previous':None}
    if not source_commit:
        path = root / '.github/update-notes.json'
        if (root / '.git').exists():
            committed = subprocess.check_output(['git','-C',str(root),'show','HEAD:.github/update-notes.json'])
            if path.read_bytes() != committed:
                raise ValueError('public update notes must be approved and committed before packaging')
        annotations = json.loads(path.read_bytes())
        validate_notes(annotations['notes'])
        if not {'en','zh-Hans','ja','ko'}.issubset(annotations['notes']):
            raise ValueError('public package notes need en/zh-Hans/ja/ko')
        result['notes'] = annotations['notes']
    else:
        if not SHA.fullmatch(source_commit):
            raise ValueError('in-house notes need a full source commit')
        result['previous'] = previous_record(output_root, build, previous)
        before = result['previous']
        count = 0
        if not before:
            subjects = ['Initial in-house build.']
        elif before['source_commit'] == source_commit:
            subjects = ['No source changes since the previous in-house build.']
        else:
            try:
                subprocess.run(['git','-C',str(root),'merge-base','--is-ancestor',before['source_commit'],source_commit],check=True,capture_output=True)
                subjects = subprocess.check_output(['git','-C',str(root),'log','--reverse','--format=%s',before['source_commit']+'..'+source_commit],text=True).splitlines()
            except (OSError, subprocess.CalledProcessError) as error:
                raise ValueError('Git history unavailable; generate a notes sidecar in the source checkout and pass --update-notes-file') from error
            count = len(subjects)
            subjects = sorted(subjects, key=lambda text: not re.match(r'^(feat|fix)(\([^)]*\))?!?:',text))
            subjects = [re.sub(r'^(?:feat|fix|chore|docs|test|refactor|perf|build|ci)(?:\([^)]*\))?!?:\s*','',text) for text in subjects]
        result['commit_count'] = count
        lines = list(dict.fromkeys(subjects));kept = []
        for line in lines:
            line = ''.join(' ' if unicodedata.category(c) == 'Cc' else c for c in line).strip()
            if len(('\n'.join([*kept,line])+'\nAdditional changes omitted.').encode()) > MAX_TEXT:
                kept.append('Additional changes omitted.');break
            if line:kept.append(line)
        result['notes'] = {'en':'\n'.join(kept) or 'No source changes since the previous in-house build.'}
    return validate(result,build,source_commit)


def write(path, value):
    raw = (json.dumps(value,ensure_ascii=False,indent=2)+'\n').encode()
    if len(raw) > MAX_FILE:
        raise ValueError('embedded notes file exceeds limit')
    Path(path).parent.mkdir(parents=True,exist_ok=True)
    Path(path).write_bytes(raw)


def embed_macos(notes_file, resources):
    resources = Path(resources);resources.mkdir(parents=True,exist_ok=True)
    shutil.copyfile(notes_file,resources/'oink-update-notes.json')


def verify_windows_embedding(tables, notes_file):
    expected = hashlib.sha256(Path(notes_file).read_bytes()).hexdigest()
    notes = [row for row in tables.get('Binary', []) if row[0] == 'OinkLocalUpdateNotes']
    if notes != [['OinkLocalUpdateNotes', expected]]:
        raise RuntimeError('MSI embedded notes differ from the generated package notes')


if __name__ == '__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--root',type=Path,default=Path(__file__).resolve().parents[1])
    p.add_argument('--build',type=int,required=True)
    p.add_argument('--source-commit',required=True)
    p.add_argument('--output-root',type=Path,required=True)
    p.add_argument('--previous-inhouse-release',type=Path)
    p.add_argument('--output',type=Path,required=True)
    a=p.parse_args();write(a.output,generate(a.root,a.build,a.source_commit,a.output_root,a.previous_inhouse_release))
