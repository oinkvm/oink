"""Keep release compiler entries separate from the development/test cache."""
from pathlib import Path
import platform
import shutil


def configure_cache(environment):
    environment = environment.copy()
    wrapper = shutil.which('sccache', path=environment.get('PATH'))
    if not wrapper:
        # sccache is optional on CI; local package hosts already provide it.
        return environment
    windows = platform.system() == 'Windows'
    if windows:
        base = Path(environment.get('LOCALAPPDATA', Path.home() / 'AppData/Local'))
    else:
        base = Path.home() / 'Library/Caches'
    directory = environment.get('OINK_PACKAGE_SCCACHE_DIR', str(base / 'oink-package-sccache'))
    port = environment.get('OINK_PACKAGE_SCCACHE_PORT', '4227')
    if not port.isdecimal() or not 1024 <= int(port) <= 65535 or int(port) == 4226:
        raise ValueError('package sccache requires a separate non-default server port in 1024..65535')
    environment.update(
        RUSTC_WRAPPER=wrapper,
        SCCACHE_DIR=str(Path(directory).expanduser().resolve()),
        SCCACHE_CACHE_SIZE=environment.get('OINK_PACKAGE_SCCACHE_SIZE', '80G' if windows else '12G'),
        SCCACHE_SERVER_PORT=port,
    )
    # A dev server's Unix socket takes precedence over the package port.
    environment.pop('SCCACHE_SERVER_UDS', None)
    return environment
