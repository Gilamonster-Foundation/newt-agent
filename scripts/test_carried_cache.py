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
                if tool == "busybox":  # upstream 403s runners; mirror fallback
                    self.assertIn(
                        f"https://github.com/Gilamonster-Foundation/newt-agent/releases/download/mirror-busybox-{version}/busybox-w64u-{version}.exe",
                        download,
                    )
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
                self.assertIn(f"-Sha256 ${hash_var}", installer)

    def test_launcher_reuses_verified_download_before_extracting(self):
        installer = (ROOT / "scripts/windows/Install-AppContainerLauncher.ps1").read_text()
        helper = (ROOT / "scripts/windows/Get-PinnedArtifact.ps1").read_text()
        carried = (ROOT / "scripts/windows/Install-CarriedTools.ps1").read_text()
        for workflow_name in ["ci.yml", "windows-appcontainer-diag.yml"]:
            workflow = (ROOT / ".github/workflows" / workflow_name).read_text()
            self.assertIn("./scripts/windows/Install-AppContainerLauncher.ps1", workflow)
            self.assertNotIn("cargo install agent-bridle-aclaunch --version", workflow)
        for script in [installer, carried]:
            self.assertIn('"$PSScriptRoot/Get-PinnedArtifact.ps1"', script)
            self.assertIn("Get-PinnedArtifact", script)
        self.assertIn("6ca23258e1bf8c3eb7881d0836c8186039c176e6f472cfe9d948acf313c4816d", installer)
        self.assertIn("agent-bridle-mcp-v0.8.0-rc.7-windows-x86_64", installer)
        self.assertLess(installer.index("Get-PinnedArtifact -"), installer.index("tar.exe -xzf"))
        self.assertIn('"$release/agent-bridle-aclaunch.exe"', installer)
        self.assertIn("if ($LASTEXITCODE -ne 0) { throw", installer)
        for flag in ["--fail", "--location", "--retry 4", "--retry-all-errors",
                     "--retry-max-time 180", "--connect-timeout 20", "--max-time 60"]:
            self.assertIn(flag, helper)
        self.assertIn("Get-FileHash -LiteralPath $Destination -Algorithm SHA256", helper)
        self.assertIn("-ne $Sha256", helper)
        self.assertIn("throw", helper)


if __name__ == "__main__":
    unittest.main()
