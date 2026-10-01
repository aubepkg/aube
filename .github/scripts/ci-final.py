#!/usr/bin/env python3
"""Aggregate the `needs` of the ci.yml `final` job.

Reads NEEDS_JSON (`toJSON(needs)`). Every job must succeed, except the
path-gated ones, which must succeed when `changes` asked for them and be
skipped when it didn't. A failed `changes` job fails the gate.
"""
import json
import os
import sys

# job name -> `changes` output that decides whether it should have run
CONDITIONAL = {"ffi": "ffi", "node-addon": "node_addon"}


def main() -> int:
    needs = json.loads(os.environ["NEEDS_JSON"])
    changes = needs["changes"]
    failed = False
    for name, data in sorted(needs.items()):
        result = data.get("result", "unknown")
        expected = "success"
        if name in CONDITIONAL and changes["result"] == "success":
            wanted = changes["outputs"].get(CONDITIONAL[name]) == "true"
            expected = "success" if wanted else "skipped"
        if result == expected:
            print(f"::notice::{name}: {result}")
        else:
            print(f"::error::{name}: {result} (expected {expected})")
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
