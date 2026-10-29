#!/usr/bin/env python3
"""Run production copy/mode commands in a private fixture, never install a driver."""
import pathlib
import shlex
import stat
import subprocess
import sys
import tempfile

root = pathlib.Path(__file__).resolve().parents[1]
installer = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else root / "install.sh"


def command(path, prefix):
    matches = [line.strip() for line in path.read_text().splitlines() if line.strip().startswith(prefix)]
    assert len(matches) == 1, (path, prefix, matches)
    return matches[0]


# Extract only the production snapshot, payload copy and permission commands.
# Running the full privileged scripts would mutate system audio, which this test
# deliberately does not do. Variables below point exclusively inside TemporaryDirectory.
clone = command(root / "privileged-bootstrap.sh", '/bin/cp -cR "$source_app"')
copy = command(installer, 'ditto "$source_bundle" "$staging/OinkMicrophone.driver"')
chmod = command(installer, 'chmod -R ')
with tempfile.TemporaryDirectory(prefix="oink-mic-permissions-") as temporary:
    base = pathlib.Path(temporary)
    app = base / "Source.app"
    relative = pathlib.Path("Contents/Resources/AudioPlugin/OinkMicrophone.driver")
    bundle = app / relative
    (bundle / "Contents/MacOS").mkdir(parents=True)
    (bundle / "Contents/Resources").mkdir()
    executable = bundle / "Contents/MacOS/OinkMicrophone"
    executable.write_bytes(b"fixture executable\n")
    (bundle / "Contents/Info.plist").write_bytes(b"fixture metadata\n")
    (bundle / "Contents/Resources/data").write_bytes(b"fixture resource\n")
    for path in [app, *app.rglob("*")]:
        path.chmod(0o755 if path.is_dir() or path == executable else 0o644)
    stage, staging = base / "snapshot", base / "publish"
    stage.mkdir(mode=0o700)
    staging.mkdir(mode=0o700)
    source_bundle = stage / "Oink.app" / relative
    variables = {"source_app": app, "stage": stage, "source_bundle": source_bundle, "staging": staging}
    setup = "\n".join(f"{key}={shlex.quote(str(value))}" for key, value in variables.items())
    subprocess.run(["/bin/bash", "-eu", "-c", setup + "\numask 077\n" + clone], check=True)
    # Reproduce the actual APFS clone regression instead of accepting a fixture
    # whose copied directories were already traversable by _coreaudiod.
    assert stat.S_IMODE(source_bundle.stat().st_mode) == 0o700
    subprocess.run(["/bin/bash", "-eu", "-c", setup + "\numask 022\n" + copy + "\n" + chmod], check=True)
    published = staging / "OinkMicrophone.driver"
    for path in [published, *published.rglob("*")]:
        relative_path = path.relative_to(published)
        original = bundle / relative_path
        expected = 0o755 if path.is_dir() or original == executable else 0o644
        actual = stat.S_IMODE(path.stat().st_mode)
        assert actual == expected, (relative_path, oct(actual), oct(expected))
        if path.is_file():
            assert path.read_bytes() == original.read_bytes()
    assert stat.S_IMODE(stage.stat().st_mode) == 0o700
    assert stat.S_IMODE(source_bundle.stat().st_mode) == 0o700
    assert stat.S_IMODE(bundle.stat().st_mode) == 0o755
print("umask 077 clone -> ditto -> production chmod: readable HAL bundle, private snapshot retained")
