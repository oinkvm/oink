"""Launch the actual macOS backend with private state and disabled desktop I/O."""
import argparse
import os
from pathlib import Path
import subprocess
import tempfile


def launch_smoke(backend, seconds=10, environment=None):
    """Fail on early exit, incomplete host/AppKit startup, or failed shutdown."""
    backend = Path(backend).resolve()
    env = dict(os.environ if environment is None else environment)
    with tempfile.TemporaryDirectory(prefix='oink-launch-smoke-') as directory:
        home = Path(directory)
        env['OINK_HOME'] = str(home)
        # Development builds also support an IPC env override. Ignore inherited
        # overrides so both debug and release smoke use port 0, never the user host.
        env.pop('OINK_IPC_PORT', None)
        with tempfile.TemporaryFile(mode='w+b') as log:
            process = subprocess.Popen([str(backend), '--launch-smoke'], env=env,
                                       stdout=log, stderr=subprocess.STDOUT)
            error = None
            try:
                try:
                    code = process.wait(timeout=seconds)
                    error = f'backend exited during {seconds}s launch smoke (exit {code})'
                except subprocess.TimeoutExpired:
                    if not (home / 'launch-smoke-ready').is_file() or not (home / 'ipc.json').is_file():
                        error = 'backend stayed alive but host/AppKit appearance startup did not complete'
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        code = process.wait(timeout=10)
                        if code != 0 and error is None:
                            error = f'backend launch smoke shutdown failed (exit {code})'
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
                        error = error or 'backend launch smoke did not stop after SIGTERM'
                log.seek(0)
                output = log.read().decode('utf-8', errors='replace')
                print(output, end='', flush=True)
            if error:
                raise RuntimeError(error)
    print(f'Backend launch smoke passed: {seconds}s, host/IPC/AppKit ready, clean shutdown', flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('backend', type=Path)
    parser.add_argument('--seconds', type=float, default=10)
    args = parser.parse_args()
    if args.seconds <= 0:
        parser.error('seconds must be positive')
    launch_smoke(args.backend, args.seconds)
