#!/usr/bin/env python3
"""Trusted release-job hooks; never invoked by renewal or untrusted PR builds.

sign-package follows published-asset/source verification (and native signature
verification when Authenticode pins are configured). The enabled trust gate is
set only after N->N+1 qualification. register runs under the feed lock. No network
API writes, arbitrary signing inputs, key overrides, or secret outputs.
"""
import argparse
import os
from pathlib import Path
import re
import feed


def release_context(config):
    source = os.environ.get('OINK_SOURCE_TREE_SHA256', '')
    tag = os.environ.get('OINK_RELEASE_TAG', '')
    run = os.environ.get('GITHUB_RUN_ID', '')
    feed.require(os.environ.get('GITHUB_ACTIONS') == 'true' and
                 os.environ.get('GITHUB_REPOSITORY') == config['repository'] and
                 re.fullmatch('[0-9a-f]{64}', source) and re.fullmatch('v[A-Za-z0-9_.-]+', tag) and
                 run.isdecimal() and int(run) > 0,
                 'release_context')
    return source, tag, int(run)


def sign_package(config, folder, output):
    source, tag, run = release_context(config)
    # Published preparation binds this exact artifact to its tag/source tree.
    # Optional native verification runs before this signing environment.
    record = feed.loads((folder / 'release.json').read_bytes().removeprefix(b'\xef\xbb\xbf'))
    feed.require(record['schema'] == 1 and record['product'] == 'Oink' and
                 record['source_tree_sha256'] == source and record['architecture'] == 'x64' and
                 record['signing'] in ('unsigned', 'signpath-approved-authenticode-verified') and
                 not record.get('update_test_feed') and
                 record['updater_included'] is True and record['collector_included'] is True and
                 type(record['build']) is int and 0 < record['build'] < 2**31 and
                 record['protocol_baseline'] == 1 and len(record['artifacts']) == 1,
                 'release_provenance')
    epoch = next(e for e in config['windows'] if e['epoch'] == 'win-2026-01')
    if epoch.get('authenticode_spki_sha256'):
        feed.require(record['signing'] == 'signpath-approved-authenticode-verified', 'authenticode_required')
    asset = record['artifacts'][0]
    feed.require(re.fullmatch('[A-Za-z0-9_.-]+[.]msi', asset['name']) and
                 0 < asset['size'] <= feed.MAX_PACKAGE, 'asset')
    raw = (folder / asset['name']).read_bytes()
    feed.require(len(raw) == asset['size'] and feed.digest(raw) == asset['sha256'], 'asset_digest')
    annotations = feed.read(folder / 'update.json')
    feed.require(set(annotations) == {'channel', 'severity', 'notes'} and
                 annotations['channel'] in ('stable', 'beta') and annotations['severity'] in ('normal', 'critical'),
                 'release_annotations')
    feed.validate_notes(annotations['notes'])
    package = {'url': f'https://github.com/{config["repository"]}/releases/download/{tag}/{asset["name"]}',
               'length': len(raw), 'sha256': feed.digest(raw), 'keyId': epoch['epoch'],
               'ed25519': feed.sign(raw, epoch['epoch'], epoch['public_key'])}
    feed.verify(raw, package['ed25519'], epoch['public_key'])
    release = {'build': record['build'], 'version': record['version'], 'tag': tag, 'sourceTreeSha256': source,
               'architecture': 'x64', 'minimumWindowsBuild': record['minimumWindowsBuild'],
               'protocolBaseline': 1, 'minimumUpdaterSchema': 1, **annotations, 'package': package}
    feed.write(output, {'schema': 1, 'approved': True, 'published': False, 'source_tree_sha256': source,
                        'workflow_run_id': run, 'signing_policy': 'authenticode-and-manifest' if epoch.get('authenticode_spki_sha256') else 'manifest-ed25519', 'release': release})


def register(config, state, record_path, now):
    record = feed.read(record_path)
    feed.require(record['schema'] == 1 and record['approved'] is True and
                 re.fullmatch('[0-9a-f]{64}', record['source_tree_sha256']) and
                 record['source_tree_sha256'] == record['release']['sourceTreeSha256'] and
                 type(record['workflow_run_id']) is int and record['workflow_run_id'] > 0 and
                 record['signing_policy'] in ('manifest-ed25519', 'authenticode-and-manifest'), 'release_provenance')
    # This binds registration to the published build. Real N->N+1 qualification
    # precedes enabling the compiled trust gate; this is not an install test.
    feed.require(os.environ.get('OINK_PUBLISHED_BUILD') == str(record['release']['build']), 'published_build_mismatch')
    record['published'] = True
    raw = feed.encode(record)
    record_hash = feed.digest(raw)
    reg = feed.registry(state)
    epoch = next(e for e in config['windows'] if e['epoch'] == record['release']['package']['keyId'])
    # A retry after sequence reservation may have a different workflow run ID.
    # Reuse the exact approved release; never append/re-sign a changed build.
    known = {h for slot in reg['feeds'][epoch['epoch']].values() for h in slot['records']}
    for existing_hash in sorted(known):
        existing = feed.read(state / 'records' / f'{existing_hash}.json')
        if existing['release']['build'] == record['release']['build']:
            feed.require(existing['published'] is True and existing['release'] == record['release'], 'release_build_changed')
            record, record_hash = existing, existing_hash
            raw = feed.encode(existing)
            break
    feed.write(state / 'records' / f'{record_hash}.json', raw)
    for channel in epoch['channels']:
        if channel == 'stable' and record['release']['channel'] != 'stable': continue
        slot = reg['feeds'][epoch['epoch']][channel]
        builds = [feed.read(state / 'records' / f'{h}.json')['release']['build'] for h in slot['records']]
        if record_hash not in slot['records']:
            feed.require(not builds or record['release']['build'] > max(builds), 'release_not_monotonic')
            slot['records'].append(record_hash)
        # Validate public HTTPS asset/hash/signature before changing the registry.
        feed.catalog(state, slot, config, epoch, channel)
    feed.write(state / 'site' / 'windows' / 'releases' / str(record['release']['build']) / 'release.json', raw)
    feed.write(state / 'registry.json', reg)
    feed.allocate(state, config, now, allow_catalog_change=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('command', choices=['sign-package', 'register'])
    args = parser.parse_args()
    config = feed.trust()
    if args.command == 'sign-package':
        sign_package(config, Path('release-input'), Path('release-output/update-record.json'))
    else:
        now = int(feed.dt.datetime.now(feed.dt.timezone.utc).timestamp())
        register(config, Path('feed-state'), Path('release-output/update-record.json'), now)


if __name__ == '__main__':
    try: main()
    except Exception as error:
        code = str(error) if type(error) is ValueError and re.fullmatch('[a-z_0-9]+', str(error)) else 'release_feed_failed'
        raise SystemExit(code)
