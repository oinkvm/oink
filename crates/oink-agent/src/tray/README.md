# Native tray artwork

`artwork.rs` defines the 16×16 macOS glyph and scaled Windows mark, badge
knockout, lock keyhole and gamepad controls. Windows uses pointer colors,
accent tail/badges and `#F04452` danger badges. Taskbar light/dark is independent
of the application's light/dark preference.

`TrayLook::of_cards` reads only the self card. Screen local mode means disabled;
unknown focus is outlined, game **active** forces solid, and badge priority is
paused > screen off > game > locked. A pause badge preserves the independently
derived pointer fill. No source mouse-mode, configured game preference, or peer
state substitutes for these facts.

Mac product PNGs contain black plus alpha only. AppKit combines the 16/32/48px
representations into one 16pt template image; the backend owns that status item.
Windows retains the existing runtime resvg pipeline for arbitrary DPI and live
accent colors. Snapshot sheets cover 16/20/24/28/32/40/48px and both taskbar themes.
Each sheet has default, cursor, locked, game, screenOff, paused, with outline then
solid for each state. Both pointer fills are kept for every artwork state.

Regenerate the checked-in Mac PNG assets and Windows pixel snapshots from vector:

```sh
OINK_UPDATE_TRAY_SNAPSHOTS=1 cargo test -p oink-agent --lib tray_pixel_snapshots
```

Normal test runs compare decoded pixels, including transparent holes. Native
status/tray visual acceptance remains an installed-build check.
