"""Exercise the GLIBC gate with symbol tables recorded from real Linux ELFs."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
FIXTURES = Path(__file__).resolve().parent / "fixtures"


class GlibcGateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.objdump = Path(self.temp.name) / "objdump"
        # Replay recorded external-tool output, not invented symbol tables.
        self.objdump.write_text('#!/bin/sh\ncat "$SYMBOL_TABLE"\n')
        self.objdump.chmod(0o755)

    def check(self, fixture, maximum, tool=None):
        env = dict(os.environ, OBJDUMP=str(tool or self.objdump))
        env["SYMBOL_TABLE"] = str(FIXTURES / fixture)
        return subprocess.run(
            [str(SCRIPTS / "check-glibc.sh"), "linux-binary", maximum],
            env=env, text=True, capture_output=True, check=False,
        )

    # Catches: the gate never inspects the symbol requirements of the release.
    def test_release_requiring_2_39_is_rejected_at_2_35(self):
        result = self.check("mdkb-v3.11.1-x64.objdump", "2.35")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("requires GLIBC_2.39 (maximum GLIBC_2.35)", result.stderr)

    # Catches: string comparisons accept 2.34 against 2.9, or reject equality.
    def test_numeric_comparison_and_inclusive_boundary(self):
        for maximum, code in [("2.35", 0), ("2.34", 0), ("2.33", 1), ("2.9", 1)]:
            with self.subTest(maximum=maximum):
                result = self.check("ubuntu-jammy-true.objdump", maximum)
                self.assertEqual(result.returncode, code, result.stderr)
                self.assertIn("requires GLIBC_2.34", result.stdout + result.stderr)

    # Catches: swallowing objdump failures silently approves an unreadable ELF.
    def test_failed_inspection_is_rejected(self):
        result = self.check("ubuntu-jammy-true.objdump", "2.35", "/usr/bin/false")
        self.assertEqual(result.returncode, 2)
        self.assertIn("Cannot inspect dynamic symbols", result.stderr)

    # Catches: an empty symbol table is mistaken for a compatible GNU binary.
    def test_missing_glibc_versions_is_rejected(self):
        self.objdump.write_text("#!/bin/sh\nexit 0\n")
        result = self.check("ubuntu-jammy-true.objdump", "2.35")
        self.assertEqual(result.returncode, 2)
        self.assertIn("No imported GLIBC symbol versions found", result.stderr)

    # Catches: malformed policy input bypasses the maximum-version comparison.
    def test_invalid_maximum_is_rejected(self):
        result = self.check("ubuntu-jammy-true.objdump", "banana")
        self.assertEqual(result.returncode, 2)
        self.assertIn("Usage:", result.stderr)


if __name__ == "__main__":
    unittest.main()
