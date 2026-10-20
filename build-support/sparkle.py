"""Fetch the pinned upstream Sparkle distribution; never install or launch it."""
import hashlib
from pathlib import Path
import tarfile
import urllib.request

VERSION = '2.9.6'
SHA256 = '52bf9e88cdd972fc0c81501377a880e90d47031bd8ca5462488f843e2609e192'
URL = f'https://github.com/sparkle-project/Sparkle/releases/download/{VERSION}/Sparkle-{VERSION}.tar.xz'


def prepare(cache):
    cache = Path(cache).resolve()
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / f'Sparkle-{VERSION}.tar.xz'
    if not archive.exists():
        urllib.request.urlretrieve(URL, archive)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
        raise RuntimeError('Sparkle archive hash mismatch')
    destination = cache / f'Sparkle-{VERSION}'
    with tarfile.open(archive) as source:
        if not destination.exists():
            destination.mkdir()
            source.extractall(destination, filter='data')
        # Never trust a previously extracted cache merely because its directory
        # exists. Compare framework AND signing/key tools to the pinned archive.
        for member in source.getmembers():
            if not member.name.startswith(('Sparkle.framework/', 'bin/')):
                continue
            path = destination / member.name
            if member.issym():
                if not path.is_symlink() or str(path.readlink()) != member.linkname:
                    raise RuntimeError('Sparkle framework symlink changed')
            elif member.isfile():
                stream = source.extractfile(member)
                if not path.is_file() or path.is_symlink() or hashlib.sha256(path.read_bytes()).digest() != hashlib.sha256(stream.read()).digest():
                    raise RuntimeError('Sparkle framework contents changed')
    if not (destination / 'Sparkle.framework').is_dir():
        raise RuntimeError('Pinned distribution does not contain Sparkle.framework')
    return destination
