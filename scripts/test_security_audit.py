"""Guard the severity policy against accidental promotion of unknown data."""

from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from scripts import security_audit
from scripts.security_audit import advisory_severity, static


class SeverityTests(unittest.TestCase):
    def test_cvss_boundaries(self):
        cases = ((0, "INFORMATIONAL"), (0.1, "LOW"), (3.9, "LOW"),
                 (4, "MEDIUM"), (6.9, "MEDIUM"), (7, "HIGH"),
                 (8.9, "HIGH"), (9, "CRITICAL"), (10, "CRITICAL"))
        for score, expected in cases:
            with self.subTest(score=score):
                self.assertEqual(advisory_severity({"cvss": score}), expected)

    def test_untrusted_cvss_formats_remain_unspecified(self):
        for score in (None, "9.8", "CVSS:3.1/AV:N", True, -1, 10.1, float("nan")):
            with self.subTest(score=score):
                self.assertEqual(advisory_severity({"cvss": score}), "UNSPECIFIED")

    def test_static_failures_are_never_downgraded(self):
        for fmt, clippy in ((1, 0), (0, 101), (1, 101)):
            checks = [{"name": name, "status": "FAIL" if code else "PASS", "exit_code": code}
                      for name, code in (("cargo-fmt", fmt), ("cargo-clippy", clippy))]
            with self.subTest(fmt=fmt, clippy=clippy), patch(
                    "scripts.security_audit.command_result", side_effect=checks), patch(
                    "scripts.security_audit.forbid_unsafe_policy",
                    return_value={"name": "forbid-unsafe-policy", "status": "PASS"}):
                self.assertEqual(static()["status"], "FAIL")


class ForbidUnsafeTests(unittest.TestCase):
    def check(self, workspace_lints: str, crate_lints: str) -> str:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text(f"[workspace]\nmembers = []\n\n{workspace_lints}")
            crate = root / "crates" / "uvcraft-x"
            crate.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(f"[package]\nname = \"uvcraft-x\"\n\n{crate_lints}")
            with patch.object(security_audit, "ROOT", root):
                return security_audit.forbid_unsafe_policy()["status"]

    def test_repository_policy_holds(self):
        self.assertEqual(security_audit.forbid_unsafe_policy()["status"], "PASS")

    def test_missing_workspace_forbid_fails(self):
        self.assertEqual(self.check("[workspace.lints.rust]\nunsafe_code = \"warn\"\n",
                                    "[lints]\nworkspace = true\n"), "FAIL")

    def test_crate_without_inherited_lints_fails(self):
        self.assertEqual(self.check("[workspace.lints.rust]\nunsafe_code = \"forbid\"\n", ""), "FAIL")

    def test_inherited_forbid_passes(self):
        self.assertEqual(self.check("[workspace.lints.rust]\nunsafe_code = \"forbid\"\n",
                                    "[lints]\nworkspace = true\n"), "PASS")


if __name__ == "__main__":
    unittest.main()
