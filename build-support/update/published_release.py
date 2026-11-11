"""Live release authorization and trusted notes for manual feed publication.

A release's channel is its live GitHub prerelease flag, read when a publisher
runs: a published prerelease is beta and enters only v2 beta catalogs; a
published non-prerelease is stable. `vX.Y.Z-beta.K` tags must stay
prereleases. A `vX.Y.Z` release may be published as a prerelease first and
promoted later by clearing the flag and dispatching again; its unchanged build
then moves from beta to stable (release.register, mac_release.candidate).
The committed notes carry only severity and the notes language map.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess

import feed

TAG = r'v(0|[1-9][0-9]*)[.](0|[1-9][0-9]*)[.](0|[1-9][0-9]*)(-beta[.][1-9][0-9]*)?'


def validate_notes_file(value):
    feed.require(type(value) is dict and set(value) == {'severity', 'notes'} and
                 value['severity'] in ('normal', 'critical'), 'release_annotations_required')
    feed.validate_notes(value['notes'])
    return value


def validate_annotations(annotations):
    """Release annotations: the live channel plus the committed severity and notes."""
    feed.require(type(annotations) is dict and set(annotations) == {'channel', 'severity', 'notes'} and
                 annotations['channel'] in ('stable', 'beta'), 'release_annotations_required')
    validate_notes_file({k: annotations[k] for k in ('severity', 'notes')})
    return annotations


def update_notes():
    # Every publisher checks out the dispatch's trusted default-branch SHA.
    # Never take notes from the release tag or its immutable package assets.
    return validate_notes_file(feed.read(feed.ROOT / '.github/update-notes.json'))


def channel(live):
    return 'beta' if live['prerelease'] is True else 'stable'


def annotations(live):
    return validate_annotations({'channel': channel(live), **update_notes()})


def published(tag, *, request=None):
    match = re.fullmatch(TAG, tag)
    feed.require(match is not None, 'release_tag')
    event = feed.read(Path(os.environ['GITHUB_EVENT_PATH']))
    kind = os.environ.get('GITHUB_EVENT_NAME')
    branch = event.get('repository', {}).get('default_branch')
    is_dispatch = (kind == 'workflow_dispatch' and branch and
                   os.environ.get('GITHUB_REF') == 'refs/heads/' + branch and
                   event.get('inputs', {}).get('release_tag') == tag)
    feed.require(os.environ.get('GITHUB_ACTIONS') == 'true' and
                 os.environ.get('GITHUB_REPOSITORY') == 'oinkvm/oink' and
                 is_dispatch, 'published_release_required')
    if request is None:
        request = lambda path: feed.loads(subprocess.check_output(['gh', 'api', path]))
    # A dispatch input does not authorize a now-draft or deleted release.
    # No tagged workflow/code is executed here.
    live = request('repos/oinkvm/oink/releases/tags/' + tag)
    feed.require(type(live.get('id')) is int and live['id'] > 0 and
                 live.get('tag_name') == tag and live.get('draft') is False and
                 type(live.get('prerelease')) is bool and live.get('published_at'), 'release_not_public')
    # A beta tag names a prerelease build; it can never be promoted to stable.
    feed.require(match[4] is None or live['prerelease'] is True, 'release_tag')
    return live


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--release-tag', required=True)
    arguments = parser.parse_args()
    live = published(arguments.release_tag)
    print(f'Verified published {annotations(live)["channel"]} release ' + arguments.release_tag)
