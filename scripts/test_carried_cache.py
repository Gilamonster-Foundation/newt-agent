#!/usr/bin/env python3
"""#2817: Windows packaging must reuse pinned downloads without trusting cache bytes.

A workflow contract test: checks the cache keys against the installer's pins,
miss-only bounded downloads, and the verified offline installation path.
Native installation still runs in the Windows job on cold and warm caches.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]


class CarriedCacheTests(unittest.TestCase):
    def test_pinned_cache_and_offline_install(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        installer = (ROOT / "scripts/windows/Install-CarriedTools.ps1").read_text()
        windows = workflow.split("\n  windows:\n", 1)[1]
        steps = re.split(r"\n      - ", windows)
        for tool, version_var, hash_var, filename in [
            ("busybox", "busyboxVersion", "busyboxSha256", "busybox.exe"),
            ("ripgrep", "version", "sha256", "ripgrep.zip"),
        ]:
            with self.subTest(tool=tool):
                version = re.search(rf"\${version_var} = '([^']+)'", installer)[1]
                digest = re.search(rf"\${hash_var} = '([^']+)'", installer)[1]
                cache = next((s for s in steps if f"id: {tool}-cache\n" in s), "")
                self.assertIn("uses: actions/cache@v4", cache)
                self.assertIn(f"key: carried-{tool}-{version}-{digest}", cache)
                self.assertNotIn("restore-keys:", cache)
                self.assertIn(
                    f"${{{{ runner.temp }}}}/carried-downloads/{filename}", cache
                )
                download = next(
                    (s for s in steps if s.startswith(f"name: Download {tool}")), ""
                )
                self.assertIn(
                    f"if: steps.{tool}-cache.outputs.cache-hit != 'true'", download
                )
                for flag in [
                    "--fail",
                    "--location",
                    "--retry 4",
                    "--retry-all-errors",
                    "--retry-max-time 180",
                    "--connect-timeout 20",
                    "--max-time 60",
                ]:
                    self.assertIn(flag, download)
                self.assertNotIn(
                    "--retry-delay", download
                )  # curl defaults to exponential backoff
                upstream = (
                    f"https://frippery.org/files/busybox/busybox-w64u-{version}.exe"
                    if tool == "busybox"
                    else f"https://github.com/BurntSushi/ripgrep/releases/download/{version}/ripgrep-{version}-x86_64-pc-windows-msvc.zip"
                )
                self.assertIn(upstream, download)
                self.assertIn(
                    f'--output "$env:RUNNER_TEMP/carried-downloads/{filename}"',
                    download,
                )
                self.assertIn("if ($LASTEXITCODE -ne 0) { throw", download)
                argument = "BusyboxArchive" if tool == "busybox" else "RipgrepArchive"
                self.assertIn(
                    f'-{argument} "$env:RUNNER_TEMP/carried-downloads/{filename}"',
                    windows,
                )
                self.assertIn(f"-ne ${hash_var}", installer)


if __name__ == "__main__":
    unittest.main()
