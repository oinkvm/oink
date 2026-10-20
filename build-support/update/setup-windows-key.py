#!/usr/bin/env python3
"""USER-ONLY: back up the epoch key in login Keychain, upload it, pin its public key.

Never run this during packaging, CI, or an agent session. No private-key file,
command argument or output is created. Enablement is unchanged.
"""
import base64
import ctypes
import json
import os
from pathlib import Path
import shutil
import shlex
import subprocess
import sys

CONFIG = Path(__file__).resolve().parents[2] / 'packaging/update-trust.json'
SECRET = 'WINDOWS_UPDATE_ED25519_PEM_WIN_2026_01'
ENVIRONMENT = 'release'
KEYCHAIN_SERVICE = 'oink-windows-update'
KEYCHAIN_ACCOUNT = 'Oink-win-2026-01'


class LoginKeychain:
    """Use Security.framework directly; password bytes never enter argv or a file."""
    def __init__(self):
        self.security = ctypes.CDLL('/System/Library/Frameworks/Security.framework/Security')
        self.core = ctypes.CDLL('/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation')
        self.core.CFRelease.argtypes = [ctypes.c_void_p]
        self.core.CFRelease.restype = None
        self.security.SecKeychainOpen.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p)]
        self.security.SecKeychainOpen.restype = ctypes.c_int32
        self.security.SecKeychainFindGenericPassword.argtypes = [
            ctypes.c_void_p, ctypes.c_uint32, ctypes.c_char_p, ctypes.c_uint32, ctypes.c_char_p,
            ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p]
        self.security.SecKeychainFindGenericPassword.restype = ctypes.c_int32
        self.security.SecKeychainAddGenericPassword.argtypes = [
            ctypes.c_void_p, ctypes.c_uint32, ctypes.c_char_p, ctypes.c_uint32, ctypes.c_char_p,
            ctypes.c_uint32, ctypes.c_void_p, ctypes.c_void_p]
        self.security.SecKeychainAddGenericPassword.restype = ctypes.c_int32
        # Resolve the login keychain explicitly, independent of the default or
        # search list. security receives no secret and its output stays captured.
        paths = shlex.split(run(['/usr/bin/security', 'login-keychain']).decode())
        if len(paths) != 1:
            raise ValueError('login_keychain_unavailable')
        self.reference = ctypes.c_void_p()
        if self.security.SecKeychainOpen(os.fsencode(paths[0]), ctypes.byref(self.reference)) != 0:
            raise ValueError('login_keychain_unavailable')

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.core.CFRelease(self.reference)

    def exists(self):
        # Null password output searches attributes only; never exports a key.
        status = self.security.SecKeychainFindGenericPassword(
            self.reference, len(KEYCHAIN_SERVICE), KEYCHAIN_SERVICE.encode(),
            len(KEYCHAIN_ACCOUNT), KEYCHAIN_ACCOUNT.encode(), None, None, None)
        if status not in (0, -25300):  # errSecItemNotFound
            raise ValueError('keychain_access_failed')
        return status == 0

    def store(self, secret):
        password = ctypes.create_string_buffer(secret)
        try:
            status = self.security.SecKeychainAddGenericPassword(
                self.reference, len(KEYCHAIN_SERVICE), KEYCHAIN_SERVICE.encode(),
                len(KEYCHAIN_ACCOUNT), KEYCHAIN_ACCOUNT.encode(), len(secret), password, None)
        finally:
            ctypes.memset(password, 0, len(password))
        if status == -25299:  # errSecDuplicateItem, including a preflight race
            raise ValueError('keychain_item_already_exists_do_not_overwrite')
        if status != 0:
            raise ValueError('keychain_backup_failed')

def run(command, data=None):
    result = subprocess.run(command, input=data, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    if result.returncode:
        raise ValueError('key_setup_command_failed')
    return result.stdout


def setup(config_path=CONFIG):
    if sys.platform != 'darwin' or os.environ.get('CI') or os.environ.get('GITHUB_ACTIONS'):
        raise ValueError('run_locally_on_key_owners_mac_outside_ci')
    config = json.loads(config_path.read_bytes())
    epoch = next(e for e in config['windows'] if e['epoch'] == 'win-2026-01')
    if epoch['public_key'] is not None or epoch['enabled']:
        raise ValueError('epoch_already_configured')
    gh, openssl = shutil.which('gh'), shutil.which('openssl')
    if gh is None or openssl is None or not run([openssl, 'version']).startswith(b'OpenSSL 3.'):
        raise ValueError('gh_and_openssl_3_required')
    scope = ['--repo', config['repository'], '--env', ENVIRONMENT]
    existing = json.loads(run([gh, 'secret', 'list', *scope, '--json', 'name']))
    if any(secret['name'] == SECRET for secret in existing):
        raise ValueError('secret_already_exists_do_not_rotate_epoch')
    with LoginKeychain() as keychain:
        if keychain.exists():
            raise ValueError('keychain_item_already_exists_do_not_overwrite')
        private = run([openssl, 'genpkey', '-algorithm', 'ED25519'])
        public = run([openssl, 'pkey', '-pubout', '-outform', 'DER'], private)
        if len(public) != 44 or public[:12] != bytes.fromhex('302a300506032b6570032100'):
            raise ValueError('unexpected_public_key')
        # Back up before uploading; a failed gh call leaves the recoverable item.
        # AddGenericPassword atomically rejects duplicates; never use an update API.
        keychain.store(private)
        run([gh, 'secret', 'set', SECRET, *scope], private)
        del private
    # Preserve the other platform's setup if it completed while Keychain/gh ran.
    config = json.loads(config_path.read_bytes())
    epoch = next(e for e in config['windows'] if e['epoch'] == 'win-2026-01')
    if epoch['public_key'] is not None or epoch['enabled']:
        raise ValueError('trust_changed_inspect_public_key_before_retrying')
    epoch['public_key'] = base64.b64encode(public[12:]).decode()
    # Only public bytes reach the working tree; never change the enabled gate.
    temporary = config_path.with_suffix('.json.tmp')
    temporary.write_text(json.dumps(config, indent=2) + '\n')
    temporary.replace(config_path)
    print('Windows public key pinned; private key backed up in login Keychain and stored in release environment. Updates remain disabled.')


if __name__ == '__main__':
    try:
        setup()
    except Exception:
        print('Windows key setup failed. A generated key remains in login Keychain if backup completed; never overwrite it. No private output was saved.', file=sys.stderr)
        raise SystemExit(1)
