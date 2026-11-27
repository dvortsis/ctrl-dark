# Ctrl+Dark mascot assets | v2.0.0

The shadow-flame mascot is a hand-reconstructed SVG interpretation of the approved concept #4, retaining the asymmetrical curling flame, detached ember, violet ribbons and almond-shaped eyes. No raster crop, AI sprite extraction or image upscaling is used for the exported production assets.

## Integration

| Use | File |
| --- | --- |
| Windows executable, window, taskbar and installer | `icons/CtrlDark.ico` |
| Notification area, dark taskbar | `tray/dark-active.ico` |
| Notification area, light taskbar | `tray/light-active.ico` |
| GitHub repository social image | `github/social-preview.png` (1280 × 640) |
| GitHub README header | `github/readme-banner.png` (1280 × 320) |
| GitHub avatar | `github/avatar.png` (512 × 512) |
| Editable vector master | `source/mascot-master.svg` |

Application icons include 16, 20, 24, 32, 40, 48, 64, 96, 128 and 256 px. The smallest frame uses its own geometry; 20–40 px use simplified geometry, and 48–256 px use the full vector master. All ICO frames are individually rendered PNGs embedded without resampling. Target Windows 10/11; actual Windows shell rendering has not been tested.

Tray icons include 16, 20, 24, 32, 40, 48 and 64 px. Choose the foreground variant according to the actual taskbar theme, not merely the application theme. `dark-*` means a light foreground for a dark taskbar; `light-*` means a dark foreground for a light taskbar. Each has `active`, `sleeping`, `disabled` and `alert` states. Sleeping uses closed eyes, disabled uses a prohibition badge, alert uses a warning triangle. State selection and accessible tooltip text are the application's responsibility. A monitor-off request does not guarantee that every display is actually off; only use the sleeping state when the intended condition is known. Unprefixed state ICOs are compatibility aliases for the dark taskbar variant; `*-taskbar.ico` aliases the corresponding active tray icon. The application taskbar icon is `icons/taskbar.ico`, identical to `icons/CtrlDark.ico`.

GitHub README illustrations and mascot variations have transparent 512 × 512 PNGs and matching SVGs. The two monochrome mascot variants are intended for contrasting backgrounds. Social images have opaque backgrounds. The 1280 × 640 social image is for GitHub Settings > General > Social preview. Embed the README banner with `![Ctrl+Dark](assets/readme-banner.png)` after copying it to your repository's assets directory.

## Rebuild

Run in a Python 3 environment with Cairo and DejaVu Sans installed. Linux example (Debian/Ubuntu):

```bash
sudo apt-get install -y python3-venv libcairo2 fonts-dejavu-core
python3 -m venv .venv
. .venv/bin/activate
python -m pip install -r requirements.txt
python build_assets.py
```

The build script contains the canonical vector geometry and emits standalone editable SVGs as well as PNG/ICO exports. Edit the geometry in `build_assets.py` to propagate design changes. SVGs are not dependent on external images; text elements use DejaVu Sans. Font/rendering differences between systems may affect byte-identical reproducibility. Run in a clean checkout or unpacked directory. `manifest.json` records SHA-256 checksums and byte sizes for delivered files, excluding itself and the runtime log. `build.log` records validation results. The manifest ignores the local virtual environment and other build caches.

## QA and limitations

- `preview/icon-size-check.png`: actual-size icons on light and dark simulated taskbars.
- `preview/16px-pixel-check.png`: nearest-neighbor 8× magnification of the actual 16 px frames.
- `preview/mascot-contact-sheet.png`: visual comparison of the full-size mascot variants.
- Validation checks all PNG decoding, expected resolution, transparent corners for icons and mascots, and ICO directory dimensions and exact embedded PNG payloads.
- The master is a deliberate vector reconstruction, not a mathematically exact trace of the original AI artwork. The smaller icons omit decorative details to prioritize legibility.
- Test icon loading, taskbar theme transitions, DPI scaling and state updates in the actual Windows application before release. This package supplies artwork, not application code.
