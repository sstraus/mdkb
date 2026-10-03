#!/usr/bin/env python3
"""Adversarial cases for scripts/check-glibc.sh (critic round 1, story 262-1329).

A stub objdump (selected through OBJDUMP) replays hand-built `objdump -T`
lines in the exact column layout of the recorded fixtures.
"""
import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check-glibc.sh"


def und(version, name="sym", flag="DF"):
    return f"0000000000000000      {flag} *UND*\t0000000000000000 ({version}) {name}"


class CheckGlibcCritic(unittest.TestCase):
    def run_gate(self, lines, maximum="2.35", stub_exit=0, objdump=None):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            out = tmp / "out.txt"
            out.write_text("\n".join(lines) + "\n")
            stub = tmp / "objdump"
            stub.write_text(f'#!/bin/sh\ncat "{out}"\nexit {stub_exit}\n')
            stub.chmod(stub.stat().st_mode | stat.S_IXUSR)
            binary = tmp / "mdkb"
            binary.write_bytes(b"x")
            env = dict(os.environ, OBJDUMP=objdump or str(stub))
            return subprocess.run(
                [str(SCRIPT), str(binary), maximum],
                env=env, capture_output=True, text=True,
            )

    # Catches: lexical comparison ("2.4" > "2.35") rejecting a compliant binary.
    def test_two_digit_minor_is_numeric_not_lexical(self):
        r = self.run_gate([und("GLIBC_2.4"), und("GLIBC_2.34")])
        self.assertEqual(r.returncode, 0, r.stderr)

    # Catches: lexical comparison accepting 2.39 against a 2.4 ceiling.
    def test_2_39_exceeds_2_4(self):
        r = self.run_gate([und("GLIBC_2.39")], maximum="2.4")
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)

    # Catches: off-by-one at the boundary (>= instead of >) and a loose 2.36 pass.
    def test_boundary_equal_passes_next_minor_fails(self):
        self.assertEqual(self.run_gate([und("GLIBC_2.35")]).returncode, 0)
        self.assertEqual(self.run_gate([und("GLIBC_2.36")]).returncode, 1)

    # Catches: dropping the third component (2.35.1 treated as 2.35).
    def test_patch_component_counts(self):
        self.assertEqual(self.run_gate([und("GLIBC_2.35.1")]).returncode, 1)
        self.assertEqual(self.run_gate([und("GLIBC_2.35")], maximum="2.35.1").returncode, 0)

    # Catches: GLIBCXX/CXXABI/GLIBC_PRIVATE being parsed as GLIBC versions.
    def test_non_glibc_version_tags_are_ignored(self):
        lines = [
            und("GLIBC_2.2.5"),
            und("GLIBCXX_3.4.30", "a"),
            und("CXXABI_1.3.13", "b"),
            und("GLIBC_PRIVATE", "c"),
            und("GLIBC_ABI_DT_RELR", "d"),
        ]
        r = self.run_gate(lines)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("GLIBC_2.2.5", r.stdout)

    # Catches: only non-GLIBC tags passing silently (fail-closed requirement).
    def test_only_glibcxx_fails_closed(self):
        r = self.run_gate([und("GLIBCXX_3.4.30"), und("GLIBC_PRIVATE", "c")])
        self.assertEqual(r.returncode, 2, r.stdout + r.stderr)

    # Catches: a static/stripped-of-dynsym binary (no output rows) passing.
    def test_no_dynamic_symbols_fails_closed(self):
        r = self.run_gate(["", "DYNAMIC SYMBOL TABLE:", "no symbols"])
        self.assertEqual(r.returncode, 2)

    # Catches: missing objdump being treated as "nothing to complain about".
    def test_missing_objdump_fails(self):
        r = self.run_gate([und("GLIBC_2.2.5")], objdump="/nonexistent/objdump")
        self.assertNotEqual(r.returncode, 0)

    # Catches: objdump failing after printing partial output still passing.
    def test_objdump_error_exit_with_partial_output_fails(self):
        r = self.run_gate([und("GLIBC_2.2.5")], stub_exit=1)
        self.assertEqual(r.returncode, 2)

    # Catches: weak undefined references (DW) escaping the check.
    def test_weak_undefined_symbol_counts(self):
        r = self.run_gate([und("GLIBC_2.2.5"), und("GLIBC_2.39", "w", flag="DW")])
        self.assertEqual(r.returncode, 1)

    # Catches: only the first/last versioned row being inspected.
    def test_offender_in_the_middle_is_found(self):
        rows = [und("GLIBC_2.2.5", "a"), und("GLIBC_2.38", "b"), und("GLIBC_2.30", "c")]
        self.assertEqual(self.run_gate(rows).returncode, 1)

    # Catches: a version not wrapped in parentheses (hidden/base form) being skipped.
    def test_unparenthesised_version_counts(self):
        row = "0000000000000000      DF *UND*\t0000000000000000  GLIBC_2.39 sym"
        self.assertEqual(self.run_gate([und("GLIBC_2.2.5"), row]).returncode, 1)

    # Catches: malformed ceilings being accepted.
    def test_bad_maximum_is_usage_error(self):
        for bad in ("2", "2.x", "GLIBC_2.35", "", "2.35 "):
            self.assertEqual(self.run_gate([und("GLIBC_2.2.5")], maximum=bad).returncode, 2, bad)


if __name__ == "__main__":
    unittest.main()
