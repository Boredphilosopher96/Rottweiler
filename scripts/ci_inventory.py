"""Package membership and projections. Workflow YAML remains the job-graph owner."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def inventory(root: Path) -> dict:
    document = json.loads((root / "contracts/package-inventory.json").read_text())
    if document.get("schema_version") != 1:
        raise ValueError("unsupported package inventory version")
    ids: set[str] = set()
    paths: set[str] = set()
    for package in document["packages"]:
        directory = PurePosixPath(package["directory"])
        if directory.is_absolute() or ".." in directory.parts or str(directory) in paths:
            raise ValueError("invalid or duplicate package directory")
        if package["id"] in ids or not package["checks"]:
            raise ValueError("duplicate package id or empty checks")
        ids.add(package["id"])
        paths.add(str(directory))
    return document


def package_manifests(root: Path) -> list[str]:
    return [package["directory"] + "/package.json" for package in inventory(root)["packages"]]


def check_inventory(root: Path) -> list[str]:
    document = inventory(root)
    errors: list[str] = []
    owned = set(package_manifests(root))
    fixtures = document["fixtures"]
    for fixture in fixtures:
        if not fixture.get("reason") or fixture["manifest"] in owned:
            errors.append("fixture requires an exclusive classification and reason")
        owned.add(fixture["manifest"])
    if len(owned) != len(document["packages"]) + len(fixtures):
        errors.append("duplicate manifest classification")
    tracked = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", "*package.json"], cwd=root,
    ).decode().split("\0")
    actual = {path for path in tracked if path and "node_modules" not in PurePosixPath(path).parts}
    errors.extend(f"unclassified package manifest: {path}" for path in sorted(actual - owned))
    errors.extend(f"missing inventory manifest: {path}" for path in sorted(owned - actual))
    for package in document["packages"]:
        manifest = json.loads((root / package["directory"] / "package.json").read_text())
        for check in package["checks"]:
            if check not in manifest.get("scripts", {}):
                errors.append(f'{package["id"]}: missing script {check}')
        if component := package.get("native_component"):
            from release_contract import load_contract
            contract = load_contract(root / "contracts/release-contract.json")
            if any(component not in {member.id for member in platform.archive_members} for platform in contract.platforms):
                errors.append(f'{package["id"]}: native component is not in every release platform')
            if "build" not in manifest.get("scripts", {}) or "build" in package["checks"]:
                errors.append(f'{package["id"]}: native build must be owned exclusively by candidate jobs')
        if not (root / package["directory"] / "bun.lock").is_file():
            errors.append(f'{package["id"]}: missing Bun lockfile')
    import yaml
    updater = yaml.load((root / ".github/dependabot.yml").read_text(), Loader=yaml.BaseLoader)
    expected = {"/" + p["directory"] for p in document["packages"]}
    observed = {u.get("directory") for u in updater["updates"] if u.get("package-ecosystem") == "bun"}
    if observed != expected:
        errors.append("Dependabot Bun coverage differs from package inventory")
    if any(u.get("package-ecosystem") == "npm" and u.get("directory") in expected for u in updater["updates"]):
        errors.append("Bun packages must use the Bun updater to maintain bun.lock")
    return errors


def load_workflow(root: Path) -> dict:
    import yaml
    return yaml.load((root / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader)


def required_jobs(workflow: dict) -> list[str]:
    return sorted(set(workflow["jobs"]) - {"required"})


def check_workflow(workflow: dict) -> list[str]:
    errors = []
    triggers = workflow.get("on", {})
    if not isinstance(triggers, dict) or not {"pull_request", "push"} <= set(triggers):
        errors.append("CI must run for pull requests and main pushes")
    elif "main" not in triggers.get("push", {}).get("branches", []):
        errors.append("CI must include main pushes")
    for settings in triggers.values() if isinstance(triggers, dict) else []:
        if isinstance(settings, dict) and ({"paths", "paths-ignore"} & set(settings)):
            errors.append("required CI cannot filter paths")
    aggregate = workflow.get("jobs", {}).get("required", {})
    if aggregate.get("if") not in ("always()", "${{ always() }}"):
        errors.append("required aggregate must use always()")
    if sorted(aggregate.get("needs", [])) != required_jobs(workflow):
        errors.append("required aggregate must depend on every CI job")
    if aggregate.get("name") != "CI required":
        errors.append("required aggregate must keep its stable check name")
    for name in ("linux-candidate-build", "macos-candidate-build"):
        job = workflow.get("jobs", {}).get(name, {})
        commands = [step.get("run", "") for step in job.get("steps", [])]
        if not any("scripts/build-native-candidate.py" in command for command in commands):
            errors.append(f"{name}: required native product build is missing")
    return errors


def require_results(expected: list[str], results: dict) -> list[str]:
    if not expected:
        return ["empty mandatory gate inventory"]
    errors = [f"unexpected gate {name}" for name in sorted(set(results) - set(expected))]
    for name in expected:
        value = results.get(name)
        result = value.get("result") if isinstance(value, dict) else None
        if result != "success":
            errors.append(f"{name}: {result or 'missing'}")
    return errors


def install(root: Path, ids: list[str], build_dependencies: bool = False, *, stdout=None) -> None:
    expected_bun = (root / ".bun-version").read_text().strip()
    actual_bun = subprocess.check_output(["bun", "--version"], text=True).strip()
    if actual_bun != expected_bun:
        raise ValueError(f"package verification requires Bun {expected_bun}, found {actual_bun}")
    packages = {p["id"]: p for p in inventory(root)["packages"]}
    directories = {str((root / p["directory"]).resolve()): p["id"] for p in packages.values()}
    installed: set[str] = set()
    built: set[str] = set()
    visiting: set[str] = set()

    def visit(name: str, dependency: bool = False) -> None:
        if name in visiting:
            raise ValueError("cyclic local package dependency")
        package = packages[name]
        directory = root / package["directory"]
        manifest = json.loads((directory / "package.json").read_text())
        if name not in installed:
            visiting.add(name)
            dependencies = manifest.get("dependencies", {}) | manifest.get("devDependencies", {})
            for spec in dependencies.values():
                if isinstance(spec, str) and spec.startswith("file:"):
                    target = str((directory / spec[5:]).resolve())
                    if target not in directories:
                        raise ValueError(f"unregistered local package dependency: {name}")
                    visit(directories[target], dependency=True)
            subprocess.run(["bun", "install", "--frozen-lockfile"], cwd=directory, check=True, **({"stdout": stdout} if stdout is not None else {}))
            visiting.remove(name)
            installed.add(name)
        if dependency and build_dependencies and name not in built and "build" in manifest.get("scripts", {}):
            subprocess.run(["bun", "run", "build"], cwd=directory, check=True, **({"stdout": stdout} if stdout is not None else {}))
            built.add(name)

    for name in ids or list(packages):
        visit(name)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["check", "matrix", "install", "package", "required"])
    parser.add_argument("packages", nargs="*")
    parser.add_argument("--build-dependencies", action="store_true",
                        help="build local package exports before installing their consumers")
    args = parser.parse_args()
    errors: list[str] = []
    if args.command == "matrix":
        print(json.dumps([p["id"] for p in inventory(ROOT)["packages"]]))
    elif args.command == "check":
        errors = check_inventory(ROOT) + check_workflow(load_workflow(ROOT))
    elif args.command == "required":
        workflow = load_workflow(ROOT)
        errors = check_workflow(workflow) + require_results(
            required_jobs(workflow), json.loads(os.environ["CI_NEEDS"]),
        )
        summary = "CI required: " + ("failed\n" + "\n".join(errors) if errors else "passed") + "\n"
        if path := os.environ.get("GITHUB_STEP_SUMMARY"):
            with open(path, "a") as stream:
                stream.write(summary)
    else:
        install(ROOT, args.packages, build_dependencies=args.build_dependencies or args.command == "package")
        if args.command == "package":
            if len(args.packages) != 1:
                raise ValueError("package verification requires exactly one package")
            package = next(p for p in inventory(ROOT)["packages"] if p["id"] == args.packages[0])
            for script in package["checks"]:
                subprocess.run(["bun", "run", script], cwd=ROOT / package["directory"], check=True)
    for error in errors:
        print(error, file=sys.stderr)
    return int(bool(errors))


if __name__ == "__main__":
    raise SystemExit(main())
