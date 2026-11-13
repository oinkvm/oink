#!/usr/bin/env python3
"""Reserve a unique in-house sequence through the public CI counter (never offline fallback)."""
import argparse
import json
from pathlib import Path
import re
import subprocess
import tempfile
import time
import uuid
from identity import encode_build
from versioning import inhouse_version, local_source_commit

REPOSITORY = 'oinkvm/oink'


def validate_receipt(value, build=None, commit=None):
    if (value.get('schema') != 1 or value.get('channel') != 'inhouse' or
            value.get('repository') != REPOSITORY or value.get('workflow') != 'ci.yml' or
            value.get('ref') != 'refs/heads/0.4' or value.get('version') != inhouse_version() or
            not re.fullmatch('[0-9a-f]{32}', value.get('request_id', '')) or
            type(value.get('run_id')) is not int or value['run_id'] <= 0):
        raise ValueError('invalid shared CI build reservation')
    expected = encode_build(inhouse_version(), value.get('sequence'))
    if value['sequence'] <= 8:
        raise ValueError('reservation predates the shared counter')
    if value.get('build') != expected or (build is not None and build != expected):
        raise ValueError('reservation build/sequence differs from package')
    if commit is not None and value.get('source_commit') != commit:
        raise ValueError('reservation belongs to another source commit; reserve a new build')
    return expected


def gh(*args):
    return subprocess.check_output(['gh', *args], text=True)


def reserve(output, commit, timeout=300):
    if output.exists():
        raise ValueError('reservation output already exists; use its number or choose a new path')
    request = uuid.uuid4().hex
    with tempfile.TemporaryDirectory(prefix='oink-reservation-') as temporary:
        directory = Path(temporary)
        body = directory / 'dispatch.json'
        body.write_text(json.dumps({'ref': '0.4', 'inputs': {'reserve_only': True, 'request_id': request}}))
        gh('api', '--method', 'POST', f'repos/{REPOSITORY}/actions/workflows/ci.yml/dispatches', '--input', str(body))
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            runs = json.loads(gh('api', f'repos/{REPOSITORY}/actions/workflows/ci.yml/runs?branch=0.4&event=workflow_dispatch&per_page=100'))['workflow_runs']
            matching = [r for r in runs if r['display_title'] == f'Oink build reservation {request}']
            if matching and matching[0]['status'] == 'completed':
                run = matching[0]
                if run['conclusion'] != 'success':
                    raise ValueError('CI reservation failed; no local number has been allocated')
                gh('run', 'download', str(run['id']), '--repo', REPOSITORY, '--name', 'build-reservation', '--dir', str(directory / 'download'))
                value = json.loads((directory / 'download/build-reservation.json').read_text())
                if value['request_id'] != request or value['run_id'] != run['id'] or value['sequence'] != run['run_number']:
                    raise ValueError('reservation artifact differs from the allocating workflow run')
                value['source_commit'] = commit
                validate_receipt(value, commit=commit)
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_text(json.dumps(value, indent=2) + '\n')
                return value
            time.sleep(3)
    raise ValueError('CI reservation timed out; stop instead of inventing a local sequence')


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--source-commit')
    a = p.parse_args()
    try:
        value = reserve(a.output, local_source_commit(a.source_commit))
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        p.exit(1, f'Build reservation failed; STOP (no local fallback): {error}\n')
    print(f"Reserved {value['version']}-inhouse.{value['sequence']} (build {value['build']})")


if __name__ == '__main__':
    main()
