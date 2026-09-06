"""Exercise HEAD's install method against an explicit verified candidate result."""
from __future__ import annotations

import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]


class HomebrewCandidateTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("ruby"), "Ruby is required for the recipe harness")
    def test_head_installs_every_verified_private_member_and_only_public_rw(self) -> None:
        contract = json.loads((REPO / "contracts/release-contract.json").read_text())
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = root / "candidate/bin"
            candidate.mkdir(parents=True)
            expected = set()
            for member in contract["archive"]["members"]:
                if not member["path"].startswith("bin/"):
                    continue
                name = member["path"].removeprefix("bin/").replace("{native_library}", "libopentui.dylib")
                expected.add(name)
                (candidate / name).write_text(member["id"])
            harness = root / "recipe.rb"
            harness.write_text(RUBY_HARNESS)
            result = subprocess.run(
                ["ruby", str(harness), str(REPO / "packaging/homebrew/rottweiler-head.rb"), str(root)],
                capture_output=True, text=True, check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            observed = json.loads(result.stdout)
            self.assertEqual(set(observed["private"]), expected)
            self.assertEqual(observed["public"], ["rw"])
            self.assertEqual(observed["calls"][0][1], "scripts/build-native-candidate.py")
            self.assertEqual(observed["calls"][1][1:],
                             ["scripts/native_candidate.py", "path", str(root / "candidate"), "engine"])
            self.assertEqual((root / "installed/libexec/rottweiler-wasm-host.identity.json").read_text(),
                             "wasm_host_identity")


RUBY_HARNESS = r'''
require "pathname"
require "fileutils"
require "json"
ROOT = Pathname(ARGV[1])
CALLS = []
module OS
  def self.linux?; false; end
end
module Utils
  def self.safe_popen_read(*args)
    CALLS << args.map(&:to_s)
    case args[1]
    when "scripts/build-native-candidate.py"
      raise "missing isolated target" unless args.include?("--target-dir")
      "#{ROOT}/candidate\n"
    when "scripts/native_candidate.py"
      raise "unverified component lookup" unless args[2..-1] == ["path", "#{ROOT}/candidate", "engine"]
      "#{ROOT}/candidate/bin/rw\n"
    else
      raise "unexpected subprocess"
    end
  end
end
class InstallPath < Pathname
  def install(paths)
    raise "candidate was not verified" unless CALLS.length == 2
    FileUtils.mkdir_p(to_s)
    paths.each { |path| FileUtils.cp(path, to_s) }
  end
  def install_symlink(path)
    FileUtils.mkdir_p(to_s)
    File.symlink(path, join(path.basename))
  end
end
class Formula
  def self.method_missing(*); end
  def self.test; end
  def self.on_linux; end
  def self.[](_); new; end
  def opt_bin; Pathname("/formula/python/bin"); end
  def buildpath; ROOT; end
  def libexec; InstallPath.new("#{ROOT}/installed/libexec"); end
  def bin; InstallPath.new("#{ROOT}/installed/bin"); end
end
load ARGV[0]
Rottweiler.new.install
puts JSON.generate({calls: CALLS, private: Dir.children("#{ROOT}/installed/libexec"),
                    public: Dir.children("#{ROOT}/installed/bin")})
'''
