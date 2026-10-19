"""Developer ID selection and notarization with no certificate subject output."""
import json
from pathlib import Path
import re
import subprocess
import time


def private_run(*args, check=True, timeout=None):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, timeout=timeout)
    if check and result.returncode:
        # Security/codesign/notary output can contain personal certificate data.
        # Keep it in memory; errors identify the operation without echoing it.
        raise RuntimeError(f'{Path(args[0]).name} operation failed (exit {result.returncode})')
    return result


def select_identity(team, fingerprint=None):
    if fingerprint and not re.fullmatch('[0-9A-Fa-f]{40}', fingerprint):
        raise ValueError('signing fingerprint must be a SHA1 certificate fingerprint')
    identities = private_run('security', 'find-identity', '-v', '-p', 'codesigning').stdout
    matches = []
    for sha, subject in re.findall(r'\b([A-Fa-f0-9]{40})\s+"([^"]+)"', identities):
        if subject.startswith('Developer ID Application:') and subject.endswith(f'({team})'):
            if fingerprint is None or sha.upper() == fingerprint.upper():
                matches.append(sha.upper())
    if len(matches) != 1:
        raise RuntimeError('Expected one existing Developer ID Application certificate for the configured Team ID; select its fingerprint explicitly if needed')
    return matches[0]


def verify_signature(path, team):
    private_run('codesign', '--verify', '--strict', '--deep', path)
    details = private_run('codesign', '-dvv', path).stderr
    if ('Authority=Developer ID Application:' not in details or f'TeamIdentifier={team}' not in details
            or 'Timestamp=' not in details or 'runtime' not in details):
        raise RuntimeError('Developer ID, hardened runtime, Team ID and secure timestamp verification failed')


def ticket_operation(operation, path, timeout=120):
    # Both staple AND validate contact Apple's ticket service (stapler(1)).
    # Accepted can precede ticket availability; EX_NOHOST also covers an
    # unexpected CloudKit response. Bound only these service/network failures,
    # never invalid/missing tickets, revoked signatures or filesystem errors.
    deadline = time.monotonic() + timeout
    delay = 2
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError(f'stapler {operation}: ticket service unavailable after {timeout}s')
        try:
            result = private_run('xcrun', 'stapler', operation, path, check=False, timeout=min(30, remaining))
            if result.returncode == 0:
                return
            if result.returncode != 68:  # EX_NOHOST; other failures are permanent.
                raise RuntimeError(f'stapler {operation} failed (exit {result.returncode})')
            # Keep certificate subjects, paths and raw Apple output private.
            errors = re.findall(r'NSURLErrorDomain\s+Code=(-?[0-9]+)', result.stdout + result.stderr)
            reason = 'exit 68' + (', network code ' + errors[0] if errors else '')
        except subprocess.TimeoutExpired:
            reason = 'ticket request timed out'
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError(f'stapler {operation}: ticket service unavailable after {timeout}s ({reason})')
        pause = min(delay, remaining)
        print(f'stapler {operation}: {reason}; waiting {pause:g}s for ticket service', flush=True)
        time.sleep(pause)
        delay = min(delay * 2, 30)


def staple_and_validate(path):
    ticket_operation('staple', path)
    ticket_operation('validate', path)


def notarize(path, profile, keychain, timeout):
    if not profile:
        return {'status': 'not-submitted'}
    args = ['--keychain-profile', profile, '--output-format', 'json']
    if keychain:
        args += ['--keychain', str(keychain)]
    result = private_run('xcrun', 'notarytool', 'submit', path, *args, check=False)
    try:
        submission = json.loads(result.stdout)
    except ValueError:
        return {'status': 'submission-failed'}
    identifier = submission.get('id')
    if result.returncode or not identifier or not re.fullmatch('[a-fA-F0-9-]{36}', identifier):
        return {'status': 'submission-failed'}
    record = {'id': identifier, 'status': submission.get('status', 'Submitted')}
    try:
        waited = private_run('xcrun', 'notarytool', 'wait', identifier, *args,
                             '--timeout', f'{timeout}s', check=False, timeout=timeout + 15)
        report = json.loads(waited.stdout)
        record['status'] = report.get('status', 'In Progress')
    except (subprocess.TimeoutExpired, ValueError):
        record['status'] = 'In Progress'
    if record['status'] == 'Accepted' and Path(path).suffix in ('.app', '.dmg'):
        staple_and_validate(path)
        record['stapled'] = True
    return record
