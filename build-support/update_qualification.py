"""Qualification transport boundary and production update-policy content gate."""
import re

REMOVED_INSTALL_POLICY = (b"waiting_for_idle", b"may_download_automatically", b"Install automatically when idle")

MARKER = b'oink-update-qualification'


def origin(value):
    if value is None:
        return None
    match = re.fullmatch(r'http://127[.]0[.]0[.]1:([0-9]{1,5})', value)
    if not match or not 1 <= int(match[1]) <= 65535:
        raise ValueError('test feed requires http://127.0.0.1:<port>, without a path')
    return value


def verify_binaries(paths, test_feed=None):
    origin(test_feed)
    for path in paths:
        data = path.read_bytes()
        if any(marker in data or marker.decode().encode('utf-16le') in data for marker in REMOVED_INSTALL_POLICY):
            raise ValueError('removed automatic-install policy found in product content')
        if not test_feed and MARKER in data:
            raise ValueError('TEST update endpoint found in production executable')
