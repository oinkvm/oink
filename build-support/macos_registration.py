"""Remove registrations created by launching a private packaging bundle."""
from contextlib import contextmanager
from pathlib import Path
import subprocess

LSREGISTER = '/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister'


def unregister_bundle(app, extension_name, extension_id):
    unregister_bundles(app, [(extension_name, extension_id)])


def unregister_bundles(app, extensions):
    app = Path(app).resolve()
    # Only disposable packaging/audit bundles are eligible, never /Applications.
    if app.suffix != '.app' or not any(p.name.endswith('.noindex') for p in app.parents):
        raise ValueError('registration cleanup requires a private .noindex app bundle')
    extensions = [(app / 'Contents/PlugIns' / name, identifier)
                  for name, identifier in extensions]

    def registrations(extension_id):
        result = subprocess.run(['pluginkit', '-mAvvv', '-i', extension_id], capture_output=True, text=True)
        if result.returncode:
            raise RuntimeError('private bundle registration verification failed')
        return result.stdout

    commands = []
    # An early launch failure may never have registered the extension. Removing
    # a nonregistered appex can fail (including read-only container audits).
    for extension, identifier in extensions:
        if str(extension) in registrations(identifier):
            commands.append(['pluginkit', '-r', str(extension)])
    # LaunchServices registers the containing app once, even when discovering
    # multiple appex bundles. Unregister it once after all explicit removals:
    # nested per-extension contexts otherwise fail on the second host removal.
    commands.append([LSREGISTER, '-u', str(app)])
    for command in commands:
        result = subprocess.run(command, capture_output=True, text=True)
        if result.returncode:
            raise RuntimeError(Path(command[0]).name + ' private bundle unregister failed')
    for _, identifier in extensions:
        if str(app) in registrations(identifier):
            raise RuntimeError('private bundle extension is still registered: ' + identifier)


@contextmanager
def temporary_registrations(app, extensions):
    try:
        yield
    finally:
        unregister_bundles(app, extensions)


@contextmanager
def temporary_registration(app, extension_name, extension_id):
    try:
        yield
    finally:
        unregister_bundle(app, extension_name, extension_id)
