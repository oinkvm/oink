#!/usr/bin/env python3
"""Shared Pages transaction steps; public-key verification only, no signing keys."""
import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import feed
import feed_site

STATE = Path('feed-state')
NEXT = Path('next-state')


def git(*args):
    return subprocess.check_output(['git', '-C', str(STATE), *args], stderr=subprocess.DEVNULL).decode().strip()


def prepare():
    platform = os.environ['PLATFORM']
    proposal = feed.read(Path('candidate-state/candidate.json'))
    feed.require(platform in feed_site.PLATFORMS and proposal['platform'] == platform, 'candidate_platform')
    reg = feed_site.state_info(STATE)
    feed.write(Path('transaction.json'), {'head': git('rev-parse', 'HEAD'), 'revision': reg['revision']})
    feed_site.merge(STATE, Path('candidate-state'), NEXT, reg['revision'])


def commit():
    fence = feed.read(Path('transaction.json'))
    feed_site.verify_public(NEXT)
    feed.require(git('rev-parse', 'HEAD') == fence['head'] and
                 feed.registry(STATE)['revision'] == fence['revision'], 'state_cas')
    # The shared workflow lock is held until readback, push and any rollback end.
    # A normal fast-forward push is a server-side compare-and-swap on this parent.
    for name in ('registry.json', 'site', 'records'):
        src, dst = NEXT / name, STATE / name
        if src.is_dir():
            shutil.copytree(src, dst, dirs_exist_ok=True)
        elif src.exists():
            shutil.copy2(src, dst)
    git('config', 'user.name', 'Oink feed automation')
    git('config', 'user.email', 'feed-automation@users.noreply.github.com')
    git('add', 'registry.json', 'site')
    if (STATE / 'records').exists():
        git('add', 'records')
    git('commit', '-m', 'Publish verified platform feed snapshot')
    try:
        git('push', 'origin', 'HEAD:update-feed-state')
    except subprocess.CalledProcessError:
        # A transport failure can occur after the server accepted the CAS. Check
        # the remote before choosing rollback, so state and hosting cannot split.
        remote = git('ls-remote', 'origin', 'refs/heads/update-feed-state').split()[0]
        if remote != git('rev-parse', 'HEAD'):
            raise


def verify_rollback():
    # Read the frozen parent, even if the local commit was prepared before a CAS failure.
    fence = feed.read(Path('transaction.json'))
    reg = feed.loads(subprocess.check_output(['git', '-C', str(STATE), 'show', f'{fence["head"]}:registry.json']))
    names = git('ls-tree', '-r', '--name-only', fence['head'], '--', 'site').splitlines()
    for name in names:
        raw = subprocess.check_output(['git', '-C', str(STATE), 'show', f'{fence["head"]}:{name}'])
        feed.require(feed.download(feed.BASE + name.removeprefix('site/'), max(feed.MAX_FEED, len(raw)), False, True) == raw,
                     'rollback_digest')
    feed_site.verify_absent(reg)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('command', choices=['prepare', 'commit', 'verify-rollback'])
    args = parser.parse_args()
    {'prepare': prepare, 'commit': commit, 'verify-rollback': verify_rollback}[args.command]()


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        code = str(error) if type(error) is ValueError and re.fullmatch('[a-z_0-9]+', str(error)) else 'deployment_failed'
        raise SystemExit(code)
