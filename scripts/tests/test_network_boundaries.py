import importlib.util
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "check-network-boundaries.py"
ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("check_network_boundaries", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ProductionSourceTests(unittest.TestCase):
    def production_source(self, source: str) -> str:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.rs"
            path.write_text(source, encoding="utf-8")
            return MODULE.production_source(path)

    def test_cfg_test_import_does_not_truncate_following_production(self) -> None:
        source = """\
#[cfg(test)]
use test_support::Server;

fn production() { TcpStream::connect(endpoint); }
"""
        production = self.production_source(source)
        self.assertNotIn("test_support", production)
        self.assertIn("TcpStream::connect", production)

    def test_cfg_test_module_is_removed_without_losing_later_items(self) -> None:
        source = """\
fn before() {}
#[cfg(test)]
mod tests {
    fn fixture() { TcpStream::connect(endpoint); }
}
fn after() { guarded_http_fetch(request); }
"""
        production = self.production_source(source)
        self.assertNotIn("TcpStream::connect", production)
        self.assertIn("fn before", production)
        self.assertIn("fn after", production)

    def test_comment_and_raw_string_braces_do_not_break_item_removal(self) -> None:
        source = '''\
#[cfg(test)]
fn fixture() {
    let _ = r#"} #[cfg(test)] {"#;
    /* } */
    TcpStream::connect(endpoint);
}
fn production() { guarded_http_fetch(request); }
'''
        production = self.production_source(source)
        self.assertNotIn("TcpStream::connect", production)
        self.assertIn("guarded_http_fetch", production)

    def test_comments_and_literals_cannot_trigger_or_satisfy_a_boundary(self) -> None:
        source = '''\
// TcpStream::connect(endpoint);
const DECOY: &str = "build_client_with_proxy_auth .send()";
fn production() { guarded_http_fetch(request); }
'''
        production = self.production_source(source)
        self.assertNotIn("TcpStream::connect", production)
        self.assertNotIn("build_client_with_proxy_auth", production)
        self.assertNotIn(".send()", production)
        self.assertIn("guarded_http_fetch", production)

    def test_neighboring_lifetimes_do_not_hide_production_code(self) -> None:
        source = "fn borrowed<'a, 'b>(left: &'a str, right: &'b str) { TcpStream::connect(endpoint); }"
        production = self.production_source(source)
        self.assertIn("TcpStream::connect", production)

    def test_cfg_all_test_item_is_removed(self) -> None:
        source = "#[cfg(all(test, unix))]\nfn fixture() { TcpStream::connect(endpoint); }\nfn live() {}"
        production = self.production_source(source)
        self.assertNotIn("TcpStream::connect", production)
        self.assertIn("fn live", production)

    def test_cfg_not_test_item_is_retained(self) -> None:
        source = "#[cfg(all(not(test), unix))]\nfn live() { TcpStream::connect(endpoint); }"
        production = self.production_source(source)
        self.assertIn("TcpStream::connect", production)

    def test_real_session_runtime_is_not_truncated_at_its_test_only_import(self) -> None:
        candidates = [
            ROOT / "crates/rw-runtime/src/session_runtime.rs",
            ROOT / "crates/rw-cli/src/session_runtime.rs",
            ROOT / "crates/rw-cli/src/runtime.rs",
        ]
        existing = [path for path in candidates if path.exists()]
        self.assertEqual(len(existing), 1, "session runtime implementation must have one owner")
        path = existing[0]
        production = MODULE.production_source(path)
        self.assertIn("compose_hosted_actor", production)
        self.assertIn("discover_runtime_extensions", production)
        self.assertGreater(len(production), 100_000)


class ManifestDependencyTests(unittest.TestCase):
    def has_reqwest(self, source: str) -> bool:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "Cargo.toml"
            path.write_text(source, encoding="utf-8")
            return MODULE._has_production_reqwest_dependency(path)

    def test_detects_inline_and_target_specific_production_dependencies(self) -> None:
        self.assertTrue(self.has_reqwest('[dependencies]\nreqwest = "0.12"\n'))
        self.assertTrue(
            self.has_reqwest(
                '[dependencies]\nprivate-http = { package = "reqwest", version = "0.12" }\n'
            )
        )
        self.assertTrue(
            self.has_reqwest(
                "[target.'cfg(unix)'.dependencies.reqwest]\nversion = \"0.12\"\n"
            )
        )

    def test_ignores_dev_only_dependency(self) -> None:
        self.assertFalse(self.has_reqwest('[dev-dependencies]\nreqwest = "0.12"\n'))

    def test_detects_workspace_renamed_dependency(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "Cargo.toml"
            path.write_text(
                "[dependencies]\nprivate-http.workspace = true\n", encoding="utf-8"
            )
            self.assertTrue(
                MODULE._has_production_reqwest_dependency(path, {"private-http"})
            )


class DirectNetworkPatternTests(unittest.TestCase):
    def test_rust_whitespace_cannot_hide_direct_construction(self) -> None:
        failures = MODULE._forbidden_direct_network(
            "fn bypass() { reqwest :: Client :: builder (); TcpStream :: connect (addr); }"
        )
        self.assertIn("reqwest Client constructor", failures)
        self.assertIn("TcpStream connection", failures)

    def test_reqwest_client_import_alias_is_rejected(self) -> None:
        failures = MODULE._forbidden_direct_network(
            "use reqwest::Client as UnsafeClient; fn bypass() { UnsafeClient::new(); }"
        )
        self.assertIn("reqwest Client import", failures)


if __name__ == "__main__":
    unittest.main()
