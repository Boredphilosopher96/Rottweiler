#!/usr/bin/env python3
"""Enforce per-platform performance baselines and reviewed PR waivers."""

from __future__ import annotations

import argparse
from decimal import Decimal, InvalidOperation, ROUND_FLOOR
import json
import re
from pathlib import Path


SCHEMA_VERSION = 1
WAIVER_LABEL = "perf-waiver"
WAIVER_HEADING = re.compile(
    r"(?ims)^#{1,6}\s*perf waiver justification\s*$\n(.*?)(?=^#{1,6}\s|\Z)"
)
PLACEHOLDER = re.compile(r"(?i)\b(?:n/?a|tbd|todo|placeholder|none)\b")


class PerformanceRegression(ValueError):
    def __init__(self, failures: list[dict[str, object]]) -> None:
        summary = "; ".join(
            f"{failure['metric']}: measured {failure['measured']}, allowed {failure['limit']}"
            for failure in failures
        )
        super().__init__(f"performance baseline regression: {summary}")
        self.failures = failures


def load(path: Path) -> dict[str, object]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"{path} must contain a JSON object")
    return value


def metric_map(value: object, label: str, *, positive: bool) -> dict[str, int]:
    if not isinstance(value, dict) or not value:
        raise ValueError(f"{label} metrics are missing")
    metrics: dict[str, int] = {}
    for name, measurement in value.items():
        if (
            not isinstance(name, str)
            or not name
            or not isinstance(measurement, int)
            or isinstance(measurement, bool)
            or measurement < (1 if positive else 0)
        ):
            raise ValueError(f"{label} metric {name!r} is invalid")
        metrics[name] = measurement
    return metrics


def baseline_suite(
    baseline: dict[str, object], platform: str, suite: str, *, require_measured: bool
) -> tuple[Decimal, str, dict[str, int]]:
    if set(baseline) != {
        "schema_version",
        "maximum_regression_fraction",
        "platforms",
    } or baseline.get("schema_version") != SCHEMA_VERSION:
        raise ValueError("performance baseline schema is invalid")
    try:
        maximum = Decimal(str(baseline["maximum_regression_fraction"]))
    except (InvalidOperation, KeyError) as error:
        raise ValueError("baseline regression fraction is invalid") from error
    if not maximum.is_finite() or maximum < 0 or maximum > Decimal("0.10"):
        raise ValueError("baseline regression fraction must be between 0 and 0.10")
    platforms = baseline.get("platforms")
    if not isinstance(platforms, dict) or platform not in platforms:
        raise ValueError(f"baseline has no platform {platform!r}")
    platform_value = platforms[platform]
    if not isinstance(platform_value, dict) or set(platform_value) != {"suites"}:
        raise ValueError(f"baseline platform {platform!r} is invalid")
    suites = platform_value["suites"]
    if not isinstance(suites, dict) or suite not in suites:
        raise ValueError(f"baseline has no suite {suite!r} for {platform!r}")
    suite_value = suites[suite]
    if not isinstance(suite_value, dict) or set(suite_value) != {
        "baseline_kind",
        "metrics",
        "provenance",
    }:
        raise ValueError(f"baseline suite {suite!r} is invalid")
    baseline_kind = suite_value["baseline_kind"]
    if baseline_kind not in {"bootstrap", "measured"}:
        raise ValueError(f"baseline suite {suite!r} has an invalid baseline kind")
    provenance = suite_value["provenance"]
    if not isinstance(provenance, str) or len(provenance.strip()) < 12:
        raise ValueError(f"baseline suite {suite!r} has no provenance")
    if require_measured and baseline_kind != "measured":
        raise ValueError(
            f"baseline suite {suite!r} for {platform!r} is bootstrap-only; "
            "reviewed measured provenance is required"
        )
    return maximum, baseline_kind, metric_map(
        suite_value["metrics"], "baseline", positive=True
    )


def measurement_metrics(value: dict[str, object], suite: str) -> dict[str, int]:
    if value.get("schema_version") == SCHEMA_VERSION and set(value) == {
        "schema_version",
        "metrics",
    }:
        return metric_map(value["metrics"], "measurement", positive=False)
    # The long-running soak already emits a bounded result document. Consume
    # its authoritative RSS value without changing the workload/output format.
    if suite == "soak" and set(value).issuperset(
        {"status", "max_rss_bytes", "rss_limit_bytes", "duration_seconds", "samples"}
    ):
        if value.get("status") != "pass":
            raise ValueError("soak measurement did not pass its absolute budget")
        return metric_map(
            {"max_rss_bytes": value.get("max_rss_bytes")},
            "measurement",
            positive=False,
        )
    raise ValueError("performance measurement schema is invalid")


def compare(
    measurements: list[dict[str, object]],
    baseline: dict[str, object],
    platform: str,
    suite: str,
    *,
    require_measured: bool = False,
) -> dict[str, object]:
    maximum, baseline_kind, expected = baseline_suite(
        baseline, platform, suite, require_measured=require_measured
    )
    observed: dict[str, int] = {}
    for document in measurements:
        for name, value in measurement_metrics(document, suite).items():
            if name in observed:
                raise ValueError(f"duplicate performance metric {name!r}")
            observed[name] = value
    if set(observed) != set(expected):
        missing = sorted(set(expected) - set(observed))
        unknown = sorted(set(observed) - set(expected))
        raise ValueError(f"performance metric set mismatch: missing={missing}, unknown={unknown}")
    checked: dict[str, object] = {}
    failures: list[dict[str, object]] = []
    for name, baseline_value in sorted(expected.items()):
        limit = int(
            (Decimal(baseline_value) * (Decimal(1) + maximum)).to_integral_value(
                rounding=ROUND_FLOOR
            )
        )
        measured = observed[name]
        checked[name] = {
            "baseline": baseline_value,
            "measured": measured,
            "limit": limit,
        }
        if measured > limit:
            failures.append(
                {"metric": name, "baseline": baseline_value, "measured": measured, "limit": limit}
            )
    if failures:
        raise PerformanceRegression(failures)
    return {
        "schema_version": SCHEMA_VERSION,
        "status": "pass",
        "platform": platform,
        "suite": suite,
        "baseline_kind": baseline_kind,
        "metrics": checked,
    }


def waiver_justification(event: dict[str, object]) -> str | None:
    pull_request = event.get("pull_request")
    if not isinstance(pull_request, dict):
        return None
    labels = pull_request.get("labels")
    if not isinstance(labels, list) or WAIVER_LABEL not in {
        label.get("name")
        for label in labels
        if isinstance(label, dict) and isinstance(label.get("name"), str)
    }:
        return None
    body = pull_request.get("body")
    if not isinstance(body, str):
        return None
    match = WAIVER_HEADING.search(body)
    if match is None:
        return None
    justification = " ".join(match.group(1).split())
    if (
        len(justification) < 80
        or len(justification.split()) < 12
        or PLACEHOLDER.search(justification) is not None
    ):
        return None
    return justification


def evaluate(
    measurements: list[dict[str, object]],
    baseline: dict[str, object],
    platform: str,
    suite: str,
    event: dict[str, object] | None = None,
    *,
    require_measured: bool = False,
) -> dict[str, object]:
    try:
        return compare(
            measurements,
            baseline,
            platform,
            suite,
            require_measured=require_measured,
        )
    except PerformanceRegression as error:
        justification = waiver_justification(event or {})
        if justification is None:
            raise
        return {
            "schema_version": SCHEMA_VERSION,
            "status": "waived",
            "platform": platform,
            "suite": suite,
            "waiver": {
                "label": WAIVER_LABEL,
                "justification_characters": len(justification),
            },
            "regressions": error.failures,
        }


def write_result(path: Path, result: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(result, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--suite", required=True, choices=("core", "soak"))
    parser.add_argument("--measurement", required=True, action="append", type=Path)
    parser.add_argument("--github-event", type=Path)
    parser.add_argument("--require-measured", action="store_true")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    result = evaluate(
        [load(path) for path in args.measurement],
        load(args.baseline),
        args.platform,
        args.suite,
        load(args.github_event) if args.github_event is not None else None,
        require_measured=args.require_measured,
    )
    if args.output is not None:
        write_result(args.output, result)
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
