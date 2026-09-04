#!/usr/bin/env python3
"""Reject unregistered/offline soak capacity before jobs enter the runner queue."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys

REQUIRED = {
    "darwin-arm64": {"self-hosted", "macOS", "ARM64", "soak"},
    "linux-x86_64": {"self-hosted", "Linux", "X64", "soak"},
}


def capacity(runners: list[dict]) -> dict[str, dict]:
    results = {}
    for platform, labels in REQUIRED.items():
        eligible = [runner for runner in runners if labels <= {label["name"] for label in runner["labels"]}]
        online = [runner for runner in eligible if runner["status"] == "online"]
        idle = [runner for runner in online if not runner["busy"]]
        state = "ready" if idle else "busy" if online else "offline" if eligible else "absent"
        results[platform] = {"state": state, "eligible_count": len(eligible), "online_count": len(online)}
    return results


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = {"schema_version": 1, "status": "infrastructure_unavailable", "platforms": {}}
    try:
        pages = json.loads(subprocess.check_output(
            ["gh", "api", "--paginate", "--slurp", f"repos/{args.repository}/actions/runners?per_page=100"],
            stderr=subprocess.PIPE,
        ))
        result["platforms"] = capacity([runner for page in pages for runner in page["runners"]])
        if all(value["state"] == "ready" for value in result["platforms"].values()):
            result["status"] = "ready"
    except (subprocess.CalledProcessError, ValueError, KeyError) as error:
        result["error"] = f"runner inventory unavailable ({type(error).__name__}); repository administration read permission is required"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, sort_keys=True) + "\n")
    print(json.dumps(result, sort_keys=True))
    return int(result["status"] != "ready")


if __name__ == "__main__":
    raise SystemExit(main())
