import importlib.util
from pathlib import Path
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
CHECKER_PATH = REPO_ROOT / "scripts" / "check-dependency-direction.py"
SPEC = importlib.util.spec_from_file_location("dependency_direction", CHECKER_PATH)
CHECKER = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(CHECKER)


class DependencyDirectionSourceContractTests(unittest.TestCase):
    def make_layout(self, root: Path) -> None:
        for crate in ("rw-core", "rw-cli", "rw-runtime"):
            (root / "crates" / crate / "src").mkdir(parents=True)
        (root / "crates" / "rw-core" / "src" / "lib.rs").write_text("")
        (root / "crates" / "rw-cli" / "src" / "main.rs").write_text("")
        (root / "crates" / "rw-cli" / "Cargo.toml").write_text(
            "[dependencies]\nrw-runtime.workspace = true\n"
        )
        runtime = root / "crates" / "rw-runtime" / "src"
        (runtime / "lib.rs").write_text("")
        for name in CHECKER.RUNTIME_COMPOSITION_FILES:
            (runtime / name).write_text("")

    def test_valid_layout_is_accepted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.make_layout(root)
            self.assertEqual(CHECKER.validate_source_layout(root), [])

    def test_facade_laundering_and_cli_duplication_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.make_layout(root)
            (root / "crates" / "rw-core" / "src" / "lib.rs").write_text(
                "pub mod runtime_support {}\n"
            )
            (root / "crates" / "rw-cli" / "src" / "session_runtime.rs").write_text("")
            failures = CHECKER.validate_source_layout(root)
            self.assertTrue(any("facade laundering" in failure for failure in failures))
            self.assertTrue(any("must not own runtime" in failure for failure in failures))

    def test_wildcard_reexport_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.make_layout(root)
            (root / "crates" / "rw-runtime" / "src" / "lib.rs").write_text(
                "pub use rw_core::*;\n"
            )
            failures = CHECKER.validate_source_layout(root)
            self.assertIn("rw-runtime must not re-export lower-layer crate APIs", failures)

    def test_selective_lower_layer_reexport_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.make_layout(root)
            (root / "crates" / "rw-runtime" / "src" / "lib.rs").write_text(
                "pub use rw_core::{EngineHost, SessionFactory};\n"
            )
            failures = CHECKER.validate_source_layout(root)
            self.assertIn("rw-runtime must not re-export lower-layer crate APIs", failures)


if __name__ == "__main__":
    unittest.main()
