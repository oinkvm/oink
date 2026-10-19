"""Remove registrations created by launching a private packaging bundle."""
from contextlib import contextmanager
from pathlib import Path
import subprocess

LSREGISTER = '/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister'


def unregister_bundle(app, extension_name, extension_id):
    app = Path(app).resolve()
    # Only disposable packaging/audit bundles are eligible, never /Applications.
    if app.suffix != '.app' or not any(p.name.endswith('.noindex') for p in app.parents):
        raise ValueError('registration cleanup requires a private .noindex app bundle')
    extension = app / 'Contents/PlugIns' / extension_name

    def registrations():
        result = subprocess.run(['pluginkit', '-mAvvv', '-i', extension_id], capture_output=True, text=True)
        if result.returncode:
            raise RuntimeError('private bundle registration verification failed')
        return result.stdout

    commands = []
    # An early launch failure may never have registered the extension. Removing
    # a nonregistered appex can fail (including read-only container audits).
    if str(extension) in registrations():
        commands.append(['pluginkit', '-r', str(extension)])
    commands.append([LSREGISTER, '-u', str(app)])
    for command in commands:
        result = subprocess.run(command, capture_output=True, text=True)
        if result.returncode:
            raise RuntimeError(Path(command[0]).name + ' private bundle unregister failed')
    if str(app) in registrations():
        raise RuntimeError('private bundle extension is still registered')


@contextmanager
def temporary_registration(app, extension_name, extension_id):
    try:
        yield
    finally:
        unregister_bundle(app, extension_name, extension_id)
