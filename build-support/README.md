# Package compilation

`package.py` calls `build.py`, which retains Tauri's frontend hook and merged
configuration. Its single Cargo build selects `oink-ui`, `oink-agent`,
`oink-crash` and, on Windows, `oink-winsvc`. Tauri adds `--bins`; all shipped
executables therefore share one feature resolution. `custom-protocol` embeds
the frontend and macOS additionally enables `oink-agent/sparkle`. The payload
role list still decides which executables ship (the Windows build also produces
an unused `oink-backend` executable).

The release builder still cleans all workspace packages and `opusic-sys` before
compiling. Source checks, native/Rust path remapping, private symbols and package
validation remain required. Registry dependencies can be restored from the
compiler cache, but old workspace mtimes cannot bypass a rebuild.

When `sccache` is available, release builds use a package-only local cache and
server, independent of the development/test cache. Defaults:

| Host | Cache directory | Limit | Server port |
| --- | --- | --- | --- |
| macOS | `~/Library/Caches/oink-package-sccache` | 12 GiB | 4227 |
| Windows | `%LOCALAPPDATA%/oink-package-sccache` | 80 GiB | 4227 |

Override with `OINK_PACKAGE_SCCACHE_DIR`, `OINK_PACKAGE_SCCACHE_SIZE` and
`OINK_PACKAGE_SCCACHE_PORT`. The dev server's default port 4226 is rejected.
Changing only `SCCACHE_DIR` does not redirect an already running server, so
package builds always select their separate port and clear inherited Unix socket
selection. To inspect statistics, run `sccache --show-stats` with the package
`SCCACHE_DIR`, `SCCACHE_CACHE_SIZE` and `SCCACHE_SERVER_PORT` in that command's
environment (including when inspecting an idle server). Stop only that server before
changing its directory or size; never stop the shared dev server.

The signed Mac smoke may register its disposable host and File Provider appex
with LaunchServices. Its `finally` cleanup unregisters only the private bundle,
unregisters the staging app and verifies that its extension path is absent,
including after a failed smoke. Installed bundle registrations are preserved.

The DMG's mounted volume uses the bundled `icon.icns`. Packaging mounts the
finished image read-only without Finder and checks the exact `.VolumeIcon.icns`
bytes and the volume's custom-icon FinderInfo flag.

The English MSI has a custom minimal UI with the existing license/background
and banner. Its welcome page offers an MSI-owned Public Desktop shortcut,
default checked for a fresh full-UI install. `DesktopFolder` resolves to Public
Desktop in this per-machine package, so the shortcut belongs to every user,
not the administrator who answers UAC. ARP and both shortcuts reference the
same bundled app ICO (16/24/32/48/64/256 pixels).

`OINK_DESKTOP_SHORTCUT=1` explicitly enables the desktop component in a quiet
install; `=0` disables it. A fresh quiet/passive install has no desktop default.
The machine registry remembers the choice for upgrades and repair, including
quiet upgrades. The welcome checkbox can change the remembered choice on a
full-UI major upgrade; transitive MSI component ownership handles repair and
uninstall. Upgrading a build without a saved choice preserves its absence.

After a successful full-UI install or major upgrade, Finish offers `Launch Oink`
and `Start Oink when I sign in`, both checked. Repair and uninstall do not offer
them. Quiet/passive and update-worker installs never invoke the finish action.
It runs only from the client Finish button, checks the actual child token
before resuming it, and refuses elevation, UIAccess and session zero. Start
Setup normally: an explicitly elevated client cannot launch Oink and receives
a warning to open it from Start instead. The short-lived app entry uses the
same OS-backed login API as Settings; unchecked means no preference edit.
The resident UI is argument-free and its existing instance lock hands off to
an already-running UI if another launch races with Finish.
