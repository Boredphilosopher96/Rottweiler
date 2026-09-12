#!/usr/bin/env python3
"""Retain local journal tracing costs against a compiled-out instrumentation build."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import platform
import statistics
import shutil
import tempfile
import subprocess
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[1]


def run(command: list[str]) -> str:
    return subprocess.run(command, cwd=ROOT, check=True, stdout=subprocess.PIPE, text=True).stdout


def summarize(sample: dict, traced: bool) -> dict:
    selected = [row for row in sample['samples'] if row['traced'] == traced]
    return {key: statistics.median(row[key] for row in selected)
            for key in ('empty_append_ns', 'page_ns', 'durable_append_ns')}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    cargo = ['cargo', 'build', '--release', '--quiet', '-p', 'rw-store',
             '--example', 'journal-trace-cost', '--message-format=json']
    # Build both before sampling, reusing this checkout's target. The temporary
    # executable copies are removed automatically; finish with ordinary features.
    with tempfile.TemporaryDirectory(prefix='rw-trace-cost-') as temporary:
        binaries = {}
        for mode, features in [('compiled_out', ['--features', 'tracing/max_level_off']),
                               ('ordinary', [])]:
            messages = [json.loads(line) for line in run(cargo + features).splitlines()]
            executable = next(message['executable'] for message in reversed(messages)
                              if message.get('reason') == 'compiler-artifact'
                              and message.get('executable'))
            destination = Path(temporary) / mode
            shutil.copy2(executable, destination)
            binaries[mode] = destination
        results = {}
        binary_hashes = {mode: hashlib.sha256(path.read_bytes()).hexdigest()
                         for mode, path in binaries.items()}
        for round_index in range(3):
            order = ('compiled_out', 'ordinary') if round_index % 2 == 0 else ('ordinary', 'compiled_out')
            for mode in order:
                sample = json.loads(run([str(binaries[mode])]))
                for row in sample['samples']:
                    row['round'] = round_index
                if mode in results:
                    results[mode]['samples'].extend(sample['samples'])
                else:
                    results[mode] = sample
        compiled_out, ordinary = results['compiled_out'], results['ordinary']
    source_files = [ROOT / 'crates/rw-store/src/session/journal.rs',
                    ROOT / 'crates/rw-store/examples/journal-trace-cost.rs']
    output = {
        'recorded_at': datetime.now(timezone.utc).isoformat(),
        'head': run(['git', 'rev-parse', 'HEAD']).strip(),
        'platform': platform.platform(),
        'rustc': run(['rustc', '-Vv']).strip(),
        'sources': {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest()
                    for path in source_files},
        'binary_sha256': binary_hashes,
        'ordinary': ordinary,
        'compiled_out': compiled_out,
        'median_ns': {
            'compiled_out': summarize(compiled_out, False),
            'disabled': summarize(ordinary, False),
            'enabled_formatted_sink': summarize(ordinary, True),
        },
        'qualification': 'Local diagnostic on a shared host. No controlled platform or p99 claim. '
                         'Enabled timing includes close-event formatting but excludes log sink IO. '
                         'Provider and model work are absent. Raw samples are retained.',
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, indent=2) + '\n')
    print(json.dumps(output['median_ns'], indent=2))


if __name__ == '__main__':
    main()
