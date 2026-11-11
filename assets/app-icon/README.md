# EpsiCut app icon

**Motif:** a hexagon holding the Greek letter epsilon (ε, the "E" of EpsiCut), drawn on the
hexagon's own 60° angles, with an amber play mark in the epsilon's open side.

## Palette

| Colour | Hex | Used for |
|---|---|---|
| Night navy | `#151a3a` to `#07080f` | the tile, with a soft violet glow behind the hexagon |
| Cyan, violet, pink | `#22d3ee`, `#8b5cf6`, `#f472b6` | the hexagon ring (diagonal gradient); violet is the app colour |
| White | `#ffffff` | the epsilon |
| Amber | `#fbbf24` | the play mark |

## Geometry and style

- A 512-unit viewBox tile, rounded square `rx=112`, full bleed.
- Rounded caps and joins everywhere. `epsicut-small.svg` has heavier strokes and no glow, for
  sizes up to 32 px.
- macOS renders (`epsicut.icns`, `epsicut-macos-512.png`) put the tile in Apple's grid (824 of
  1024 px, transparent margin). Windows, Linux and the web use the full-bleed tile.

## Provenance

Drawn for EpsiCut by its owner with Claude, after a hexagon sketch the owner supplied. Licence:
[LICENSE.txt](LICENSE.txt) (MIT OR Apache-2.0).

## Files

| File | What |
|---|---|
| `epsicut.svg` | canonical artwork |
| `epsicut-small.svg` | heavier strokes for 16–32 px |
| `epsicut-1024.png` | 1024 px render |
| `epsicut-macos-512.png` | runtime Dock icon on macOS (embedded by `apps/epsicut/src/main.rs`) |
| `epsicut.icns` | macOS bundle icon (`CFBundleIconFile`) |
| `epsicut.ico` | Windows icon, 16–256 px (embedded in `epsicut.exe` by `apps/epsicut/build.rs`) |
| `hicolor/<size>/apps/ai.storyteller.epsicut.png`, `hicolor/scalable/…svg` | Linux icon theme, used by `packaging/linux/ai.storyteller.epsicut.desktop` |

The web app's `apps/epsicut-web/web/favicon.png` (128 px) comes from the same artwork.

## How the icon reaches each OS

- **macOS:** the window icon set at startup (`ViewportBuilder::with_icon`) is the Dock and app-switcher
  icon when the app runs unbundled. A future `.app` bundle points `CFBundleIconFile` at `epsicut.icns`.
- **Windows:** `build.rs` embeds `epsicut.ico` as the exe's resource icon (Explorer, Start menu,
  pinned taskbar), and the runtime icon covers the window, taskbar and Alt-Tab.
- **Linux:** the runtime icon plus `with_app_id("ai.storyteller.epsicut")`, which Wayland matches to
  the `.desktop` file. Install the `hicolor/` tree under `/usr/share/icons/hicolor/` and the desktop
  file under `/usr/share/applications/`.

## Regenerate

`packaging/icons.sh` (needs `resvg`; `iconutil` on macOS for the `.icns`; the `.ico` is packed by
`cargo xtask ico`). To change the artwork, replace `epsicut.svg` and `epsicut-small.svg`, then run the script. Every derived file needs its `.attribution` sidecar
and a row in `ATTRIBUTION.md`.
