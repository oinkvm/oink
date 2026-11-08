"""Validate incoming MSI paths using the product-root policy used since 0.3.0."""
import re


def relative_file(directory, name, directories):
    if '/' in name:
        raise ValueError('embedded file separator')
    parts, seen = [name], set()
    while directory != 'INSTALLFOLDER':
        if directory in seen or directory not in directories:
            raise ValueError('file is outside INSTALLFOLDER or has cyclic ancestry')
        seen.add(directory)
        directory, part = directories[directory]
        if '/' in part:
            raise ValueError('embedded directory separator')
        parts.append(part)
    path = '/'.join(reversed(parts))
    # Match the supported inspector: exact receipt exception, case-folded reserved paths.
    if path != 'installed.json':
        if not path or len(path) > 240 or not path.isascii() or '\\' in path or ':' in path:
            raise ValueError('invalid resource path')
        for part in path.split('/'):
            stem = part.split('.')[0].upper()
            if (not re.fullmatch(r'[A-Za-z0-9._-]+', part) or part in ('.', '..')
                    or part.endswith(('.', ' ')) or stem in ('CON', 'PRN', 'AUX', 'NUL')
                    or re.fullmatch(r'(COM|LPT)[1-9]', stem)):
                raise ValueError('invalid resource component')
        if path.lower() in ('files.json','installed.json','maintenance') or path.lower().startswith('maintenance/'):
            raise ValueError('reserved resource path')
    return path


def verify_file_table(tables):
    """Inspect actual MSI File/Component/Directory rows, never source filenames."""
    # Keep extraction below INSTALLFOLDER; authenticated future resource names
    # are intentionally not constrained by the installed version's inventory.
    directories = {row[0]: (row[1], row[2].split('|')[-1])
                   for row in tables['Directory']}
    components = {row[0]: row[2] for row in tables['Component']}
    if not tables['File'] or len(tables['File']) > 4096:
        raise RuntimeError('MSI contains no File rows')
    paths, errors = [], []
    for row in tables['File']:
        try:
            paths.append(relative_file(components[row[1]], row[2].split('|')[-1], directories))
        except (KeyError, ValueError) as error:
            errors.append(f'{row[0]} ({row[2]}): {error}')
    if errors:
        raise RuntimeError('Invalid MSI product-root inventory: ' + '; '.join(errors))
    if len(set(path.lower() for path in paths)) != len(paths):
        raise RuntimeError('Duplicate case-insensitive MSI path')
    return paths
