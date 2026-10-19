#!/usr/bin/env python3
"""Trusted feed writer. Registry is data; never run code from update-feed-state.

The caller holds oink-update-feed-publish while reserving Windows sequences.
Allocate must be committed before sign; a failed sequence is never reused.
No command accepts a private key, URL, epoch override or release catalog on argv.
"""
import argparse
import base64
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unicodedata
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
BASE = 'https://updates.oink.hachiwii.com/'
MAX_FEED = 1024 * 1024
MAX_PACKAGE = 512 * 1024 * 1024
MAX_NOTES = 16 * 1024
KEY_ENV = {'win-2026-01': 'WINDOWS_UPDATE_ED25519_PEM_WIN_2026_01'}


def require(ok, code):
    if not ok:
        raise ValueError(code)


def validate_notes(notes):
    require(type(notes) is dict and set(notes) == {'zh-Hans', 'en'}, 'notes_languages')
    for text in notes.values():
        require(isinstance(text, str) and bool(text.strip()) and len(text.encode('utf-8')) <= MAX_NOTES and
                not any(unicodedata.category(c) == 'Cc' and c not in '\n\t' for c in text), 'notes_text')
    return notes


def unique(pairs):
    obj = {}
    for key, value in pairs:
        require(key not in obj, 'duplicate_key')
        obj[key] = value
    return obj


def loads(raw):
    require(len(raw) <= MAX_FEED, 'metadata_limit')
    return json.loads(raw, object_pairs_hook=unique, parse_constant=lambda _: require(False, 'number'))


def read(path):
    return loads(path.read_bytes())


def encode(value):
    return json.dumps(value, ensure_ascii=False, separators=(',', ':'), allow_nan=False).encode()


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    raw = value if isinstance(value, bytes) else encode(value)
    temp = path.with_suffix('.tmp')
    with temp.open('wb') as stream:
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())
    temp.replace(path)


def timestamp(value):
    parsed = dt.datetime.fromisoformat(value.replace('Z', '+00:00'))
    require(parsed.tzinfo is not None, 'timestamp')
    return int(parsed.timestamp())


def iso(value):
    return dt.datetime.fromtimestamp(value, dt.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')


def openssl(args):
    # Never forward tool stderr (it may contain key material or local paths).
    result = subprocess.run(['openssl', *args], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    require(result.returncode == 0, 'crypto')
    return result.stdout


def public_der(key):
    raw = base64.b64decode(key, validate=True)
    require(len(raw) == 32, 'public_key')
    return bytes.fromhex('302a300506032b6570032100') + raw


def verify(raw, signature, key):
    sig = base64.b64decode(signature, validate=True)
    require(len(sig) == 64, 'signature_length')
    with tempfile.TemporaryDirectory() as directory:
        folder = Path(directory)
        (folder / 'pub').write_bytes(public_der(key))
        (folder / 'sig').write_bytes(sig)
        (folder / 'data').write_bytes(raw)
        openssl(['pkeyutl', '-verify', '-pubin', '-keyform', 'DER', '-inkey', str(folder / 'pub'),
                 '-rawin', '-in', str(folder / 'data'), '-sigfile', str(folder / 'sig')])


def sign(raw, epoch, public_key):
    secret = os.environ.get(KEY_ENV[epoch])
    require(secret is not None, 'secret_unavailable')
    with tempfile.TemporaryDirectory() as directory:
        folder = Path(directory)
        key = folder / 'key.pem'
        fd = os.open(key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'w') as stream:
            stream.write(secret)
        derived = openssl(['pkey', '-in', str(key), '-pubout', '-outform', 'DER'])
        require(derived == public_der(public_key), 'secret_public_mismatch')
        (folder / 'data').write_bytes(raw)
        signature = openssl(['pkeyutl', '-sign', '-inkey', str(key), '-rawin', '-in', str(folder / 'data')])
    require(len(signature) == 64, 'signature_length')
    return base64.b64encode(signature).decode()


class Redirects(urllib.request.HTTPRedirectHandler):
    max_redirections = 5
    max_repeats = 1
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        from urllib.parse import urlsplit
        url = urlsplit(newurl)
        require(url.scheme == 'https' and url.hostname in
                ('release-assets.githubusercontent.com', 'objects.githubusercontent.com')
                and not url.username and not url.password and url.port is None and not url.fragment, 'redirect')
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def download(url, limit, redirects=False):
    opener = urllib.request.build_opener(Redirects() if redirects else NoRedirects())
    with opener.open(urllib.request.Request(url, headers={'User-Agent': 'Oink-feed-verifier/1'}), timeout=60) as response:
        require(response.status == 200, 'http_status')
        result = response.read(limit + 1)
        require(len(result) <= limit, 'download_limit')
        return result


class NoRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args):
        raise ValueError('feed_redirect')


def trust(platform='windows'):
    config = read(ROOT / 'packaging/update-trust.json')
    require(config['schema'] == 1 and config['repository'] == 'oinkvm/oink', 'trust_schema')
    if platform == 'windows':
        require(config['windows'], 'platform_state')
        for epoch in config['windows']:
            require(type(epoch['enabled']) is bool and epoch['epoch'] in KEY_ENV, 'platform_state')
            if not epoch['enabled']:
                continue
            require(epoch['epoch'] in KEY_ENV and epoch['public_key'] and
                    epoch['authenticode_spki_sha256'] and all(
                        re.fullmatch('[0-9a-f]{64}', pin) for pin in epoch['authenticode_spki_sha256']),
                    'trust_unconfigured')
            public_der(epoch['public_key'])
        config['windows'] = [e for e in config['windows'] if e['enabled']]
    elif platform == 'mac':
        require(type(config['mac']['enabled']) is bool, 'platform_state')
        if config['mac']['enabled']:
            require(config['mac']['epoch'] == 'mac-2026-01' and config['mac']['public_key'], 'trust_unconfigured')
            public_der(config['mac']['public_key'])
    else:
        raise ValueError('platform')
    return config


def manifest(raw, epoch, channel, allow_expired=False, now=None):
    now = int(dt.datetime.now(dt.timezone.utc).timestamp()) if now is None else now
    envelope = loads(raw)
    require(set(envelope) == {'envelopeSchema', 'payload', 'signatures'} and envelope['envelopeSchema'] == 1, 'envelope')
    payload = base64.b64decode(envelope['payload'], validate=True)
    signatures = envelope['signatures']
    require(1 <= len(signatures) <= 4 and len({s['keyId'] for s in signatures}) == len(signatures), 'signatures')
    signature = next(s for s in signatures if s['keyId'] == epoch['epoch'])
    verify(payload, signature['ed25519'], epoch['public_key'])
    obj = loads(payload)
    require(obj['schema'] == 1 and obj['product'] == 'com.hachiwii.oink' and
            obj['keyEpoch'] == epoch['epoch'] and obj['channel'] == channel and
            type(obj['sequence']) is int and obj['sequence'] > 0, 'manifest_identity')
    start, end = timestamp(obj['generatedAt']), timestamp(obj['expiresAt'])
    require(0 < end - start <= 30 * 86400 and start <= now + 300, 'manifest_clock')
    require(allow_expired or end > now, 'manifest_expired')
    require(len(obj['releases']) <= 32, 'release_count')
    for release in obj['releases']:
        validate_notes(release['notes'])
    return obj


def relative(epoch, channel):
    require(re.fullmatch(r'win-[0-9]{4}-[0-9]{2}', epoch) and channel in ('stable', 'beta'), 'feed_path')
    return f'windows/v1/{epoch}/{channel}/manifest.json'


def registry(folder):
    obj = read(folder / 'registry.json')
    require(obj['schema'] == 1, 'registry_schema')
    return obj


def catalog(state, slot, config, epoch, channel):
    releases = []
    for record_hash in slot['records']:
        require(re.fullmatch('[0-9a-f]{64}', record_hash), 'record_hash')
        raw = (state / 'records' / f'{record_hash}.json').read_bytes()
        require(digest(raw) == record_hash, 'record_digest')
        record = loads(raw)
        require(record['schema'] == 1 and record['approved'] is True and record['published'] is True and
                record['source_tree_sha256'] == record['release']['sourceTreeSha256'] and
                re.fullmatch('[0-9a-f]{64}', record['source_tree_sha256']) and
                type(record['workflow_run_id']) is int and record['workflow_run_id'] > 0 and
                bool(record['signing_approval']), 'unapproved_record')
        release = record['release']
        validate_notes(release['notes'])
        require(channel == 'beta' or release.get('channel', 'stable') == 'stable', 'release_channel')
        package = release['package']
        require(re.fullmatch('[A-Za-z0-9_.-]+', release['tag']) and release['tag'] not in ('.', '..'), 'tag')
        prefix = f'https://github.com/{config["repository"]}/releases/download/{release["tag"]}/'
        require(package['url'].startswith(prefix) and
                re.fullmatch('[A-Za-z0-9_.-]+[.]msi', package['url'][len(prefix):]) and
                package['keyId'] == epoch['epoch'] and 0 < package['length'] <= MAX_PACKAGE, 'asset')
        raw = download(package['url'], package['length'], True)
        require(len(raw) == package['length'] and digest(raw) == package['sha256'], 'asset_digest')
        verify(raw, package['ed25519'], epoch['public_key'])
        releases.append(release)
    require(len(releases) <= 32 and len({r['build'] for r in releases}) == len(releases), 'catalog')
    return releases


def allocate(state, config, now, allow_catalog_change=False):
    reg = registry(state)
    require(config["windows"], "platform_disabled")
    plans = []
    for epoch in config['windows']:
        for channel in epoch['channels']:
            slot = reg['feeds'][epoch['epoch']][channel]
            path = relative(epoch['epoch'], channel)
            releases = catalog(state, slot, config, epoch, channel)
            current = state / 'site' / path
            if current.exists():
                raw = current.read_bytes()
                require(digest(raw) == slot['manifest_sha256'], 'snapshot_digest')
                old = manifest(raw, epoch, channel, True, now)
                require(old['sequence'] <= slot['last_allocated'], 'allocation_watermark')
                changed = old['releases'] != releases
                require(not changed or allow_catalog_change, 'renewal_catalog_changed')
                if not changed and timestamp(old['expiresAt']) - now > 21 * 86400:
                    continue
            else:
                require(slot['last_allocated'] == 0 and not slot.get('manifest_sha256'), 'snapshot_missing')
            slot['last_allocated'] += 1
            payload = {'schema': 1, 'product': 'com.hachiwii.oink', 'keyEpoch': epoch['epoch'],
                       'channel': channel, 'sequence': slot['last_allocated'], 'generatedAt': iso(now),
                       'expiresAt': iso(now + 30 * 86400), 'releases': releases}
            raw = encode(payload)
            plans.append({'epoch': epoch['epoch'], 'channel': channel, 'path': path,
                          'payload': base64.b64encode(raw).decode(), 'payload_sha256': digest(raw)})
    write(state / 'registry.json', reg)
    write(state / 'pending-renewal.json', {'schema': 1, 'plans': plans})


def sign_plans(state, config):
    require(config['windows'] and all(os.environ.get(KEY_ENV[e['epoch']]) for e in config['windows']), 'secret_unavailable')
    reg = registry(state)
    for plan in read(state / 'pending-renewal.json')['plans']:
        epoch = next(e for e in config['windows'] if e['epoch'] == plan['epoch'])
        raw = base64.b64decode(plan['payload'], validate=True)
        obj = loads(raw)
        slot = reg['feeds'][plan['epoch']][plan['channel']]
        require(digest(raw) == plan['payload_sha256'] and obj['sequence'] == slot['last_allocated'] and
                plan['path'] == relative(plan['epoch'], plan['channel']), 'allocation_changed')
        signature = sign(raw, epoch['epoch'], epoch['public_key'])
        envelope = encode({'envelopeSchema': 1, 'payload': plan['payload'],
                           'signatures': [{'keyId': epoch['epoch'], 'ed25519': signature}]})
        manifest(envelope, epoch, plan['channel'])
        write(state / 'site' / plan['path'], envelope)
        slot['manifest_sha256'] = digest(envelope)
        slot['verified_at'] = None
    write(state / 'registry.json', reg)



def verify_site(state, config, public=False):
    reg = registry(state)
    for epoch in config['windows']:
        for channel in epoch['channels']:
            slot = reg['feeds'][epoch['epoch']][channel]
            path = relative(epoch['epoch'], channel)
            raw = download(BASE + path, MAX_FEED) if public else (state / 'site' / path).read_bytes()
            require(digest(raw) == slot['manifest_sha256'], 'deployment_digest')
            obj = manifest(raw, epoch, channel)
            require(obj['sequence'] <= slot['last_allocated'], 'deployment_sequence')
            if public:
                slot['verified_at'] = iso(int(dt.datetime.now(dt.timezone.utc).timestamp()))
    if public:
        write(state / 'registry.json', reg)


def verify_packages(state, config):
    reg = registry(state)
    for epoch in config['windows']:
        for channel in epoch['channels']:
            obj = manifest((state / 'site' / relative(epoch['epoch'], channel)).read_bytes(), epoch, channel)
            releases = catalog(state, reg['feeds'][epoch['epoch']][channel], config, epoch, channel)
            require(obj['releases'] == releases, 'signed_catalog_changed')


def monitor(config, seen_path):
    if not config["windows"]:
        print("Windows feed disabled; expiry monitor skipped.")
        return
    now = int(dt.datetime.now(dt.timezone.utc).timestamp())
    seen = read(seen_path) if seen_path.exists() else {}
    level = 0
    for epoch in config['windows']:
        for channel in epoch['channels']:
            path = relative(epoch['epoch'], channel)
            raw = download(BASE + path, MAX_FEED)
            obj = manifest(raw, epoch, channel, True, now)
            previous = seen.get(path, {'sequence': 0, 'hash': ''})
            require(obj['sequence'] >= previous['sequence'] and
                    (obj['sequence'] != previous['sequence'] or digest(raw) == previous['hash']), 'monitor_replay')
            remaining = timestamp(obj['expiresAt']) - now
            level = max(level, 2 if remaining <= 7 * 86400 else 1 if remaining <= 14 * 86400 else 0)
            seen[path] = {'sequence': obj['sequence'], 'hash': digest(raw)}
    heartbeat = read_public_heartbeat()
    require(-300 <= now - timestamp(heartbeat['checked_at']) <= 48 * 3600, 'heartbeat_stale')
    write(seen_path, seen)
    require(level == 0, f'expiry_alert_{level}')


def read_public_heartbeat():
    return loads(download(BASE + 'heartbeat.json', MAX_FEED))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('command', choices=['allocate', 'sign', 'verify', 'verify-public', 'monitor', 'verify-packages'])
    parser.add_argument('--state', type=Path, default=Path('feed-state'))
    args = parser.parse_args()
    config = trust()
    now = int(dt.datetime.now(dt.timezone.utc).timestamp())
    if args.command == 'allocate': allocate(args.state, config, now)
    elif args.command == 'sign': sign_plans(args.state, config)
    elif args.command == 'verify-packages': verify_packages(args.state, config)
    elif args.command in ('verify', 'verify-public'): verify_site(args.state, config, args.command == 'verify-public')
    elif args.command == 'monitor': monitor(config, args.state / 'monitor-seen.json')


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        # Values are untrusted; only a bounded internal category leaves this process.
        code = str(error) if type(error) is ValueError and re.fullmatch('[a-z_0-9]+', str(error)) else 'feed_operation_failed'
        raise SystemExit(code)
