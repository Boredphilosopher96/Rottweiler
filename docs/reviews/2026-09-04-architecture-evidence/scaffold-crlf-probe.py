#!/usr/bin/env python3
"""Reproduce the production scaffold parser's CRLF handling in a temporary copy.

Exit zero means the defect is reproduced, not that the product is correct.
Requires Bun; does not install dependencies or modify production files.
"""
from pathlib import Path
import json
import shutil
import subprocess
import tempfile

repo = Path(__file__).resolve().parents[3]
sdk = repo / "packages/plugin-sdk"
with tempfile.TemporaryDirectory(prefix="rw-scaffold-crlf-") as directory:
    copied = Path(directory)
    (copied / "src").mkdir()
    shutil.copyfile(sdk / "src/scaffold.ts", copied / "src/scaffold.ts")
    shutil.copytree(sdk / "fixtures/scaffold", copied / "fixtures/scaffold")
    mapping = copied / "fixtures/scaffold/files.txt"
    original = mapping.read_bytes().replace(b"\r\n", b"\n")

    def paths():
        result = subprocess.run(
            ["bun", "--eval",
             "const m = await import(process.argv[1]); "
             "console.log(JSON.stringify(m.renderTypeScriptScaffold().map(f => f.path)))",
             str(copied / "src/scaffold.ts")],
            check=True, capture_output=True, text=True,
        )
        return json.loads(result.stdout)

    mapping.write_bytes(original)
    lf = paths()
    mapping.write_bytes(original.replace(b"\n", b"\r\n"))
    crlf = paths()
    assert all("\r" not in path for path in lf), lf
    assert any("\r" in path for path in crlf), crlf
    assert [path.rstrip("\r") for path in crlf] == lf, (lf, crlf)
    print(json.dumps({"status": "defect reproduced", "lf": lf, "crlf": crlf}))
