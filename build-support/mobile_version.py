#!/usr/bin/env python3
"""PC/phone labels share versioning rules; signed/local 0.4 builds use CI reservations."""
import argparse
import json
from pathlib import Path
import re
import sys
from identity import decode_build
from versioning import inhouse_label, beta_label, local_source_commit


def mobile_version(build, channel='inhouse'):
    if channel not in ('inhouse', 'beta'):
        raise ValueError('unknown mobile package channel')
    return {'build': build, 'version_name': beta_label(build) if channel == 'beta' else inhouse_label(build),
            'marketing_version': decode_build(build)[0]}


def parse_build(text):
    if not re.fullmatch(r'[0-9]+', text):
        raise ValueError(f'build must be decimal digits, not {text!r}')
    return int(text)


def main(argv):
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('build', type=parse_build)
    p.add_argument('--channel', choices=['inhouse', 'beta'], default='inhouse')
    p.add_argument('--reservation', type=Path)
    p.add_argument('--xcode', action='store_true')
    a = p.parse_args(argv)
    try:
        fields = mobile_version(a.build, a.channel)
        if a.channel == 'inhouse' and decode_build(a.build)[0] == '0.4.0':
            if not a.reservation:
                raise ValueError('new 0.4 in-house phone builds require --reservation from the shared CI counter')
            from build_reservation import validate_receipt
            receipt = json.loads(a.reservation.read_text())
            validate_receipt(receipt, a.build, local_source_commit(receipt.get('source_commit')))
    except (ValueError, OSError) as error:
        print(f'mobile_version: {error}', file=sys.stderr)
        return 1
    if a.xcode:
        print(f"MARKETING_VERSION={fields['marketing_version']} CURRENT_PROJECT_VERSION={fields['build']} OINK_DISPLAY_VERSION={fields['version_name']}")
    else:
        print(json.dumps(fields))
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
