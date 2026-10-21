"""Find private markers in public source paths and bytes without echoing values.

Keep the marker JSON outside the source tree. This is a local export-review aid;
its findings need review alongside credential and address checks.
"""
import argparse
import json
import os
from pathlib import Path
import re

EXCLUDED_DIRS = {'.git', '.claude', 'target', 'node_modules', 'dist', 'build', 'evidence', '.release-tools', '__pycache__'}
ROOT_EXCLUDED = {'docs', 'scripts', 'tools', 'AGENTS.md', 'CLAUDE.md'}


def source_files(root):
    # Match the public export boundary, including nested READMEs that are not
    # compiler/provenance inputs. Never follow source links outside the export.
    for directory, dirs, files in os.walk(root, followlinks=False):
        here = Path(directory)
        dirs[:] = sorted(d for d in dirs if d not in EXCLUDED_DIRS and not (here == root and d in ROOT_EXCLUDED))
        links = [d for d in dirs if (here / d).is_symlink()]
        dirs[:] = [d for d in dirs if d not in links]
        for name in sorted(files + links):
            if name == '.DS_Store' or name in EXCLUDED_DIRS or (here == root and name in ROOT_EXCLUDED):
                continue
            yield here / name


def scan(root, markers):
    if not isinstance(markers, list) or not markers or any(not isinstance(m, str) or not m for m in markers):
        raise ValueError('markers must be a nonempty list of nonempty strings')
    expressions = [re.compile(re.escape(marker.encode('utf-8')), re.IGNORECASE) for marker in markers]
    root = Path(root)
    findings = []
    for path in source_files(root):
        relative = path.relative_to(root).as_posix()
        data = os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
        for marker, expression in enumerate(expressions):
            # Identifier underscores and camel-case text are not privacy boundaries.
            # Scan file names too: a fixture can disclose a machine alias in its path.
            if expression.search(relative.encode('utf-8')):
                findings.append(dict(path=relative, scope='path', marker=marker, line=None))
            for match in expression.finditer(data):
                findings.append(dict(path=relative, scope='content', marker=marker,
                                     line=data[:match.start()].count(b'\n') + 1))
    return findings


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path.cwd())
    parser.add_argument('--markers', type=Path, required=True, help='Private local JSON string array, outside the source tree')
    parser.add_argument('--output', type=Path, help='Optional local JSON findings file, outside the source tree')
    args = parser.parse_args()
    root = args.root.resolve()
    if args.markers.resolve().is_relative_to(root) or (args.output and args.output.resolve().is_relative_to(root)):
        parser.error('keep private marker configuration and review output outside the source tree')
    findings = scan(root, json.loads(args.markers.read_text()))
    result = json.dumps(dict(status='review_required' if findings else 'passed', findings=findings), indent=2) + '\n'
    if args.output:
        args.output.write_text(result)
    else:
        print(result, end='')
    raise SystemExit(bool(findings))


if __name__ == '__main__':
    main()
