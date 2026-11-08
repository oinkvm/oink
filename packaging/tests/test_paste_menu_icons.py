"""The native shell menus ship the generated VI mark in both package paths."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
MAC_RESOURCES = ROOT / "apps/finder-sync/Resources"
WINDOWS_RC = ROOT / "packaging/windows/PasteMenu.rc"
PACKAGE = ROOT / "packaging/windows/Package.wxs"
GENERATOR = ROOT / "tools/generate-paste-menu-icons.py"


class PasteMenuIcons(unittest.TestCase):
    def test_generated_assets_have_all_menu_sizes_and_theme_variants(self):
        for stem in ("OinkPasteMenu", "OinkPasteMenuDark"):
            for size in (16, 32):
                self.assertTrue((MAC_RESOURCES / f"{stem}{'@2x' if size == 32 else ''}.png").is_file())
        for name in ("OinkPasteMenu.ico", "OinkPasteMenuDark.ico"):
            data = (ROOT / "packaging/windows" / name).read_bytes()
            self.assertEqual(data[:4], b"\x00\x00\x01\x00")
            self.assertGreaterEqual(int.from_bytes(data[4:6], "little"), 4)
            count = int.from_bytes(data[4:6], "little")
            widths = [data[6 + 16 * i] or 256 for i in range(count)]
            self.assertEqual(widths, [16, 20, 24, 32, 40, 48, 64])

    def test_resources_are_generated_from_vi_sources_and_embedded_in_dll(self):
        self.assertTrue((MAC_RESOURCES / "mark.svg").read_text().find("#FF6F91") >= 0)
        self.assertTrue((MAC_RESOURCES / "mark-reversed.svg").is_file())
        self.assertIn("rsvg-convert", GENERATOR.read_text())
        rc = WINDOWS_RC.read_text()
        self.assertIn("#define IDI_PASTE_MENU_LIGHT 101", rc)
        self.assertIn("#define IDI_PASTE_MENU_DARK 102", rc)
        self.assertRegex(rc, r'IDI_PASTE_MENU_LIGHT\s+ICON\s+"OinkPasteMenu\.ico"')
        self.assertRegex(rc, r'IDI_PASTE_MENU_DARK\s+ICON\s+"OinkPasteMenuDark\.ico"')
        self.assertIn('Source="$(var.Payload)/OinkPasteMenu.dll"', PACKAGE.read_text())


if __name__ == "__main__":
    unittest.main()
