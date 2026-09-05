import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from ci_api_result import summarize


class ApiResultTests(unittest.TestCase):
    def setUp(self):
        self.needs = {
            name: {
                "result": "success",
                "outputs": {"results_present": "true", "checks_passed": "true"},
            }
            for name in ("rust-api-aube", "rust-api-resolver", "rust-api-core", "c-abi")
        }

    def test_all_checks_pass(self):
        self.assertEqual(summarize(self.needs), (True, True))

    def test_advisory_failure_survives_continue_on_error(self):
        for result in ("success", "failure"):
            with self.subTest(result=result):
                self.needs["rust-api-core"]["result"] = result
                self.needs["rust-api-core"]["outputs"]["checks_passed"] = "false"
                self.assertEqual(summarize(self.needs), (True, False))

    def test_missing_setup_timeout_or_cancelled_result_blocks(self):
        for name in self.needs:
            for replacement in (
                None,
                {"result": "failure", "outputs": {}},
                {"result": "success", "outputs": {}},
                {"result": "cancelled", "outputs": self.needs[name]["outputs"]},
                {"result": "skipped", "outputs": self.needs[name]["outputs"]},
                {"result": "success", "outputs": {"results_present": "true"}},
            ):
                with self.subTest(name=name, replacement=replacement):
                    needs = copy.deepcopy(self.needs)
                    if replacement is None:
                        del needs[name]
                    else:
                        needs[name] = replacement
                    self.assertEqual(summarize(needs), (False, False))

    def test_cli_writes_outputs_even_when_a_check_fails(self):
        self.needs["c-abi"]["outputs"]["checks_passed"] = "false"
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            result = subprocess.run(
                [sys.executable, str(Path(__file__).with_name("ci_api_result.py"))],
                env={**os.environ, "NEEDS_JSON": json.dumps(self.needs), "GITHUB_OUTPUT": str(output)},
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 1)
            self.assertEqual(output.read_text(), "results_present=true\nchecks_passed=false\n")


if __name__ == "__main__":
    unittest.main()
