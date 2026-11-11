#!/usr/bin/env python3
"""Commit allocation metadata only. Never publish unsigned or unverified site bytes."""
from pathlib import Path
import feed
from deploy import git

reg = feed.registry(Path('feed-state'))
reg['revision'] += 1
feed.write(Path('feed-state/registry.json'), reg)
git('config', 'user.name', 'Oink feed automation')
git('config', 'user.email', 'feed-automation@users.noreply.github.com')
# v1 Windows plans and per-platform v2 plans; a Mac-only state has no v1 file.
git('add', 'registry.json', *sorted(path.name for path in Path('feed-state').glob('pending-*.json')))
if Path('feed-state/records').exists():
    git('add', 'records')
git('commit', '-m', 'Reserve update feed sequences')
git('push', 'origin', 'HEAD:update-feed-state')
