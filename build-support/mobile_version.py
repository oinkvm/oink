#!/usr/bin/env python3
"""Phone package version fields from a build, by the PC packages' rule (versioning.inhouse_label).

    mobile_version.py <build>          JSON for Gradle: build, version_name, marketing_version
    mobile_version.py --xcode <build>  xcodebuild settings: MARKETING_VERSION=… CURRENT_PROJECT_VERSION=…

A build that does not encode inhouse_version is rejected (exit 1). Leading zeros
are ignored: 000400001 is build 400001.
"""
import json
import re
import sys

from identity import decode_build
from versioning import inhouse_label


def mobile_version(build):
    """Android versionCode / iOS CFBundleVersion is the build; the label and marketing version follow it."""
    label = inhouse_label(build)
    return {'build': build, 'version_name': label, 'marketing_version': decode_build(build)[0]}


def parse_build(text):
    if not re.fullmatch(r'[0-9]+', text):
        raise ValueError(f'build must be decimal digits, not {text!r}')
    return int(text)


def main(argv):
    xcode = argv[:1] == ['--xcode']
    args = argv[1:] if xcode else argv
    if len(args) != 1:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    try:
        fields = mobile_version(parse_build(args[0]))
    except ValueError as error:
        print(f'mobile_version: {error}', file=sys.stderr)
        return 1
    if xcode:
        print(f"MARKETING_VERSION={fields['marketing_version']} CURRENT_PROJECT_VERSION={fields['build']}")
    else:
        print(json.dumps(fields))
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
