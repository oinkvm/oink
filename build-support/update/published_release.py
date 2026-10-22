"""Live stable-release authorization and trusted notes for manual feed publication."""
import argparse
import os
from pathlib import Path
import re
import subprocess

import feed


def validate_annotations(annotations):
    feed.require(type(annotations) is dict and set(annotations) == {'channel', 'severity', 'notes'} and
                 annotations['channel'] == 'stable' and annotations['severity'] in ('normal', 'critical'),
                 'stable_annotations_required')
    feed.validate_notes(annotations['notes'])
    return annotations


def update_notes():
    # Every publisher checks out the dispatch's trusted default-branch SHA.
    # Never take notes from the release tag or its immutable package assets.
    return validate_annotations(feed.read(feed.ROOT / '.github/update-notes.json'))


def published(tag, *, request=None):
    feed.require(re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+', tag), 'release_tag')
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
    # A dispatch input does not authorize a now-draft,
    # deleted or prerelease release. No tagged workflow/code is executed here.
    live = request('repos/oinkvm/oink/releases/tags/' + tag)
    feed.require(type(live.get('id')) is int and live['id'] > 0 and
                 live.get('tag_name') == tag and live.get('draft') is False and
                 live.get('prerelease') is False and live.get('published_at'), 'release_not_public')
    return live


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--release-tag', required=True)
    arguments = parser.parse_args()
    published(arguments.release_tag)
    update_notes()
    print('Verified published stable release ' + arguments.release_tag)
