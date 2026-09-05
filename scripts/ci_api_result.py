"""Combine API check outputs without hiding a failed setup or missing shard."""

import json
import os
from pathlib import Path


def summarize(needs):
    expected = {"rust-api-aube", "rust-api-resolver", "rust-api-core", "c-abi"}
    present = set(needs) == expected and all(
        check.get("result") in {"success", "failure"}
        and check.get("outputs", {}).get("results_present") == "true"
        and check.get("outputs", {}).get("checks_passed") in {"true", "false"}
        for check in needs.values()
    )
    passed = present and all(
        check["result"] == "success"
        and check["outputs"]["checks_passed"] == "true"
        for check in needs.values()
    )
    return present, passed


if __name__ == "__main__":
    needs = json.loads(os.environ["NEEDS_JSON"])
    present, passed = summarize(needs)
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
        output.write(f"results_present={str(present).lower()}\n")
        output.write(f"checks_passed={str(passed).lower()}\n")
    for name, check in sorted(needs.items()):
        print(f"{name}: {check.get('result', 'unknown')} {check.get('outputs', {})}")
    if not present:
        print("::error::One or more API checks did not report a result")
    elif not passed:
        print("::error::API compatibility check failed; inspect the individual check logs")
    raise SystemExit(0 if passed else 1)
