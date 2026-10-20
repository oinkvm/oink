#!/usr/bin/env python3
"""USER-RUN ONLY: retain a Sparkle key in Keychain and send it to GitHub stdin.

Never invoke this from CI or an agent. Only the public key is written to the
checkout. Qualification/enablement is a separate reviewed change.
"""
import argparse
import base64
import json
import os
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from sparkle import prepare

ROOT = Path(__file__).resolve().parents[2]
ACCOUNT = 'Oink-mac-2026-01'
SECRET = 'SPARKLE_ED25519_PRIVATE_KEY'
REPOSITORY = 'oinkvm/oink'


def private_run(command, **kwargs):
    result = subprocess.run(command, capture_output=True, **kwargs)
    if result.returncode:
        # Keychain/tool output must not reach logs, including exception strings.
        raise RuntimeError(f'{Path(command[0]).name} failed; no trust changes written')
    return result.stdout


def setup(cache):
    if sys.platform != 'darwin' or os.environ.get('CI') or os.environ.get('GITHUB_ACTIONS'):
        raise RuntimeError('run locally on the key owner\'s Mac, outside CI')
    config_path = ROOT / 'packaging/update-trust.json'
    config = json.loads(config_path.read_bytes())
    private_run(['gh', 'auth', 'status'])
    private_run(['gh', 'api', f'repos/{REPOSITORY}/environments/release'])
    tool = str(prepare(cache) / 'bin/generate_keys')
    # Official Sparkle generation is idempotent for this dedicated account;
    # the private seed stays backed up in the user's encrypted login Keychain.
    private_run([tool, '--account', ACCOUNT])
    public = private_run([tool, '--account', ACCOUNT, '-p']).decode().strip()
    if len(base64.b64decode(public, validate=True)) != 32:
        raise RuntimeError('invalid Sparkle public key')
    if config['mac']['public_key'] not in (None, public):
        raise RuntimeError('existing public key differs; key rotation needs review')
    # generate_keys -x writes atomically to a file, so it cannot export to a
    # pipe. Read this one Keychain item directly into memory instead. Never use
    # a shell, argv, a temporary file or inherited stdout for the private seed.
    secret = private_run(['security', 'find-generic-password', '-s',
                          'https://sparkle-project.org', '-a', ACCOUNT, '-w']).strip()
    if len(base64.b64decode(secret, validate=True)) not in (32, 96):
        raise RuntimeError('invalid Sparkle private key format')
    private_run(['gh', 'secret', 'set', SECRET, '--repo', REPOSITORY,
                 '--env', 'release'], input=secret)
    del secret
    # Preserve a concurrently edited Windows slot; never enable either platform
    # as a side effect of key provisioning, or replace an existing public root.
    config = json.loads(config_path.read_bytes())
    if config['mac']['public_key'] not in (None, public):
        raise RuntimeError('trust changed; inspect the public key before retrying')
    config['mac']['public_key'] = public
    temporary = config_path.with_suffix('.json.tmp')
    temporary.write_text(json.dumps(config, indent=2) + '\n')
    temporary.replace(config_path)
    print(f'Public key written; {SECRET} set in {REPOSITORY}/release. Enablement unchanged.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--tool-cache', type=Path,
                        default=Path.home() / 'Library/Caches/oink-release-tools')
    args = parser.parse_args()
    try:
        setup(args.tool_cache)
    except Exception:
        # Do not print tracebacks/tool output from a secret-handling command.
        raise SystemExit('Key setup failed; private key remains in Keychain if generated. No private output was saved. Check GitHub authorization and retry.') from None


if __name__ == '__main__':
    main()
