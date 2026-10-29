"""Build gate for the immutable Windows updater shipped in Oink 0.2.2.

Both public 67c272655 and main-derived 8c07d26ba enforce this same allow-list.
The current service accepts more resources, but cannot change those readers.
"""
import re

FILES = {
    'oink.exe', 'oink-agent.exe', 'oink-service.exe', 'oink-update-worker.exe',
    'oink-crash-collector.exe', 'installed.json', 'oink-build.json',
    'report-manifest.json', 'third-party-licenses.json', 'THIRD_PARTY_LICENSES.md',
    'NOTICE', 'LICENSE', 'LICENSE-ADDITIONAL-TERMS.md',
    'AudioPlugin/VBCABLE_Driver_Pack45.zip',
}


def relative_file(directory, name, directories):
    parts, seen = [name], set()
    while directory != 'INSTALLFOLDER':
        if directory in seen or directory not in directories:
            raise ValueError('file is outside INSTALLFOLDER or has cyclic ancestry')
        seen.add(directory)
        directory, part = directories[directory]
        parts.append(part)
    path = '/'.join(reversed(parts))
    # Match 0.2.2 package::validate_path, including its installed.json exception.
    if path != 'installed.json':
        if not path or len(path) > 240 or not path.isascii() or '\\' in path or ':' in path:
            raise ValueError('invalid resource path')
        for part in path.split('/'):
            stem = part.split('.')[0].upper()
            if (not re.fullmatch(r'[A-Za-z0-9._-]+', part) or part in ('.', '..')
                    or part.endswith(('.', ' ')) or stem in ('CON', 'PRN', 'AUX', 'NUL')
                    or re.fullmatch(r'(COM|LPT)[1-9]', stem)):
                raise ValueError('invalid resource component')
        if path == 'files.json' or path.startswith('maintenance/'):
            raise ValueError('reserved resource path')
    if path not in FILES and not path.startswith('licenses/'):
        raise ValueError('path not admitted by the shipped 0.2.2 updater')
    return path


def verify_file_table(tables):
    """Inspect actual MSI File/Component/Directory rows, never source filenames."""
    # Shipped 0.2.2 updaters reject the WHOLE package on any unlisted File row.
    # Keep this gate even though today's service validator is directory-based.
    directories = {row[0]: (row[1], row[2].split('|')[-1])
                   for row in tables['Directory']}
    components = {row[0]: row[2] for row in tables['Component']}
    if not tables['File']:
        raise RuntimeError('MSI contains no File rows')
    paths, errors = [], []
    for row in tables['File']:
        try:
            paths.append(relative_file(components[row[1]], row[2].split('|')[-1], directories))
        except (KeyError, ValueError) as error:
            errors.append(f'{row[0]} ({row[2]}): {error}')
    if errors:
        raise RuntimeError('MSI is incompatible with shipped 0.2.2 updaters: ' + '; '.join(errors))
    return paths
