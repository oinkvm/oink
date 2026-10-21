# Windows native menu icons

These SVG snapshots come from the rendered in-app `DeviceMenu` and `Glyph`,
including the installed `oink-ui` icon CSS. The source consistency test prevents
native artwork from silently diverging when frontend icons change.

Regenerate from the repository root after installing the locked dependencies:

```sh
OINK_UPDATE_MENU_ICONS=1 npm test -w apps/oink -- native-menu-icons
```

Review the SVG changes and run the same test without the environment variable.
The Windows renderer fits the SVG into the monitor's native menu-check metrics,
uses the native Menu theme text colors (system colors in high contrast), and
installs premultiplied BGRA DIBs with `MIIM_BITMAP`. It does not enable owner draw
or replace check/radio marks. Theme and DPI changes refresh active menus; each
new popup also checks its current monitor and theme. macOS is unaffected.

Hands-on acceptance: on Windows, open the tray menu in light/dark and high-contrast
themes at 100%, 125%, 150% and 200% scaling, including switching monitors. Check
icon legibility, alignment, disabled items, check marks, and submenu behavior.
Automated raster/theme tests and an unattached HMENU test do not establish visual
acceptance. Desktop automation on the Windows test host is not supported by the
current device-test guard, so visual acceptance remains a user hands-on item.

## Source and licence review

The inventory pins every exported SVG by SHA-256. The following mapping was
checked against the installed, lockfile-pinned `oink-ui@1.0.5` bundle and licence,
its `src/core/icons.ts` (icons exported from its design symbols), and the Oink
source/history (`6370ebf8` imports the device artwork; `e8327604` adds the open
lock and eye-off menu variants). No Lucide import or Lucide-derived path was
identified in these source definitions. The separate Lucide `fold-horizontal`
asset remains independently attributed in the native inventory.

| Export | Shape source | Inventory licence |
| --- | --- | --- |
| `audio.svg` | Oink `Glyph('speaker')`, including computed wave arcs | first-party GPL-3.0-only |
| `game.svg` | Oink `Glyph('game')` / `GAMEPAD` | first-party GPL-3.0-only |
| `keyboard.svg` | Oink `Glyph('keyboard')` | first-party GPL-3.0-only |
| `mouse.svg` | Oink `Glyph('mouse')` | first-party GPL-3.0-only |
| `pause.svg` | Oink `DeviceMenu.PauseIcon` | first-party GPL-3.0-only |
| `quit.svg` | Oink `DeviceMenu.PowerIcon` | first-party GPL-3.0-only |
| `hide-screens.svg` | Oink `DeviceMenu.EyeOffIcon` (custom full eye and slash, not the library's broken eye-off path) | first-party GPL-3.0-only |
| `lock.svg` | `oink-ui` `ICONS.lock` | MIT |
| `resume.svg` | `oink-ui` `ICONS.play` | MIT |
| `show.svg` | `oink-ui` `ICONS.external` | MIT |
| `show-screens.svg` | `oink-ui` `ICONS.eye` | MIT |
| `unlock.svg` | Oink `DeviceMenu.UnlockIcon`; retains the library lock body rectangle and changes its shackle | GPL-3.0-only AND MIT |

The four library glyphs preserve their geometry; serialization inlines the
library's Icon CSS. Oink's inline menu shapes use the same Icon presentation
attributes; the library's MIT terms are retained in the npm attribution and in
`licenses/native/oink-ui-1.0.5-LICENSE.txt`. The device Glyph shapes instead use
Oink's `devices.css`. Changing presentation does not transfer ownership of the
underlying shapes.

Before merging any new or regenerated asset: update its inventory/provenance,
run `node build-support/generate-licenses.mjs --check`, regenerate only if drift
is reported, then require `--check`, `npm run licenses:test` and the native icon
consistency test to pass.
