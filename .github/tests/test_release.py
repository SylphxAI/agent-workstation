"""Exercise the release preflight without network access or publication rights."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "release-check.sh"


class ReleaseCheck(unittest.TestCase):
    def run_check(self, *, version="0.1.0", code="404", curl_status=0, git_status=2):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stubs = {
                "cargo": "echo '{\"packages\":[{\"version\":\"0.1.0\"}]}'",
                "curl": 'printf "%s" "$HTTP_CODE"; exit "$CURL_STATUS"',
                "git": 'exit "$GIT_STATUS"',
            }
            for name, body in stubs.items():
                path = root / name
                path.write_text("#!/bin/sh\n" + body + "\n")
                path.chmod(0o755)
            output = root / "output"
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                       REQUESTED_VERSION=version, HTTP_CODE=code,
                       CURL_STATUS=str(curl_status), GIT_STATUS=str(git_status),
                       GITHUB_OUTPUT=str(output))
            result = subprocess.run(["bash", str(SCRIPT)], env=env,
                                    capture_output=True, text=True, check=False)
            return result.returncode, output.read_text() if output.exists() else ""

    def test_unpublished_version_can_publish(self):
        status, output = self.run_check()
        self.assertEqual(status, 0)
        self.assertEqual(output, "version=0.1.0\npublish=true\n")

    def test_published_version_skips_even_when_tag_exists(self):
        status, output = self.run_check(code="200", git_status=0)
        self.assertEqual(status, 0)
        self.assertEqual(output, "version=0.1.0\npublish=false\n")

    def test_version_must_match(self):
        for version in ("", "0.2.0", "$(echo unexpected)", "0.1.0\npublish=true"):
            with self.subTest(version=version):
                status, output = self.run_check(version=version)
                self.assertNotEqual(status, 0)
                self.assertEqual(output, "")

    def test_registry_errors_fail_closed(self):
        for code in ("000", "301", "401", "403", "429", "500", "503"):
            with self.subTest(code=code):
                status, output = self.run_check(code=code)
                self.assertNotEqual(status, 0)
                self.assertNotIn("publish=true", output)

    def test_network_failure_fails_closed(self):
        status, output = self.run_check(code="404", curl_status=28)
        self.assertNotEqual(status, 0)
        self.assertNotIn("publish=true", output)

    def test_existing_unpublished_tag_fails_closed(self):
        status, output = self.run_check(git_status=0)
        self.assertNotEqual(status, 0)
        self.assertNotIn("publish=true", output)

    def test_tag_read_error_fails_closed(self):
        status, output = self.run_check(git_status=128)
        self.assertNotEqual(status, 0)
        self.assertNotIn("publish=true", output)


if __name__ == "__main__":
    unittest.main()
