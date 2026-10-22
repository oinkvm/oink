#!/usr/bin/env python3
"""Prepare Windows feed inputs from a published release, using trusted code only."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import feed
import published_release

sys.path.insert(0, str(feed.ROOT / 'build-support'))
from provenance import source_tree_sha256


def gh_json(*arguments):
    return feed.loads(subprocess.check_output(['gh', *arguments]))


def prepare(config, output):
    tag = os.environ['OINK_RELEASE_TAG']
    live = published_release.published(tag, request=lambda path: gh_json('api', path))
    assets = live['assets']
    def asset(name, limit):
        matches = [a for a in assets if a['name'] == name]
        url = f'https://github.com/{config["repository"]}/releases/download/{tag}/{name}'
        feed.require(len(matches) == 1 and matches[0]['browser_download_url'] == url and
                     0 < matches[0]['size'] <= limit, 'published_asset')
        raw = feed.download(url, limit, True)
        feed.require(len(raw) == matches[0]['size'], 'asset_length')
        return raw
    combined = feed.loads(asset('release.json', feed.MAX_FEED))
    record = combined['platforms']['windows']
    feed.require(combined['schema'] == 1 and combined['product'] == 'Oink' and
                 not record.get('update_test_feed') and
                 tag == 'v' + combined['version'] and
                 all(record[k] == combined[k] for k in ('build', 'version', 'built_commit', 'source_tree_sha256')) and
                 re.fullmatch('[0-9a-f]{40}', record['built_commit']) and
                 re.fullmatch('[0-9a-f]{64}', record['source_tree_sha256']), 'release_provenance')
    # Fetch data only. Never execute the tagged tree's scripts or workflows.
    subprocess.run(['git', 'fetch', '--no-tags', '--depth=1', 'origin', f'refs/tags/{tag}'], check=True, stdout=subprocess.DEVNULL)
    commit = subprocess.check_output(['git', 'rev-parse', 'FETCH_HEAD^{commit}'], text=True).strip()
    feed.require(commit == record['built_commit'], 'release_tag_commit')
    with tempfile.TemporaryDirectory() as temporary:
        tree = Path(temporary) / 'source'
        subprocess.run(['git', 'worktree', 'add', '--detach', str(tree), commit], check=True, stdout=subprocess.DEVNULL)
        try:
            feed.require(source_tree_sha256(tree) == record['source_tree_sha256'], 'release_source_tree')
        finally:
            subprocess.run(['git', 'worktree', 'remove', '--force', str(tree)], check=True, stdout=subprocess.DEVNULL)
    feed.require(len(record['artifacts']) == 1, 'windows_asset_count')
    package = record['artifacts'][0]
    feed.require(package in combined['artifacts'] and re.fullmatch('[A-Za-z0-9_.-]+[.]msi', package['name']), 'asset')
    raw = asset(package['name'], feed.MAX_PACKAGE)
    feed.require(len(raw) == package['size'] and feed.digest(raw) == package['sha256'], 'asset_digest')
    output.mkdir(parents=True, exist_ok=True)
    feed.write(output / 'release.json', record)
    feed.write(output / package['name'], raw)
    with open(os.environ['GITHUB_OUTPUT'], 'a') as environment:
        environment.write(f'source_tree_sha256={record["source_tree_sha256"]}\n')
        environment.write(f'build={record["build"]}\n')


if __name__ == '__main__':
    try:
        prepare(feed.trust(), Path('release-input'))
    except Exception as error:
        code = str(error) if type(error) is ValueError and re.fullmatch('[a-z_0-9]+', str(error)) else 'published_release_failed'
        raise SystemExit(code)
