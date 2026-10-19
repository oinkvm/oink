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
git('add', 'registry.json', 'pending-renewal.json')
if Path('feed-state/records').exists():
    git('add', 'records')
git('commit', '-m', 'Reserve Windows feed sequences')
git('push', 'origin', 'HEAD:update-feed-state')
