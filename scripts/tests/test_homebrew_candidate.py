"""Exercise HEAD's install method against an explicit verified candidate result."""
from __future__ import annotations

import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import homebrew_toolchains

REPO = Path(__file__).resolve().parents[2]


class HomebrewCandidateTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("ruby"), "Ruby is required for the recipe harness")
    def test_head_installs_every_verified_private_member_and_only_public_rw(self) -> None:
        for platform in ("darwin", "linux"):
            with self.subTest(platform=platform):
                self.assert_candidate_install(platform)

    def assert_candidate_install(self, platform: str) -> None:
        contract = json.loads((REPO / "contracts/release-contract.json").read_text())
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = root / "candidate/bin"
            candidate.mkdir(parents=True)
            expected = set()
            for member in contract["archive"]["members"]:
                if not member["path"].startswith("bin/"):
                    continue
                native = "libopentui.so" if platform == "linux" else "libopentui.dylib"
                name = member["path"].removeprefix("bin/").replace("{native_library}", native)
                expected.add(name)
                (candidate / name).write_text(member["id"])
            tools = homebrew_toolchains.manifest(REPO, "Linux" if platform == "linux" else "Darwin", "x86_64" if platform == "linux" else "arm64")
            (root / "tools.json").write_text(json.dumps(tools))
            harness = root / "recipe.rb"
            harness.write_text(RUBY_HARNESS)
            result = subprocess.run(
                ["ruby", str(harness), str(REPO / "packaging/homebrew/rottweiler-head.rb"), str(root), platform],
                capture_output=True, text=True, check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            observed = json.loads(result.stdout)
            self.assertEqual(set(observed["private"]), expected)
            self.assertEqual(observed["public"], ["rw"])
            self.assertEqual(observed["calls"][0][1], "scripts/homebrew_toolchains.py")
            self.assertEqual(observed["calls"][1][1], "scripts/build-native-candidate.py")
            self.assertEqual(observed["calls"][2][1:],
                             ["scripts/native_candidate.py", "path", str(root / "candidate"), "engine"])
            self.assertEqual((root / "installed/libexec/rottweiler-wasm-host.identity.json").read_text(),
                             "wasm_host_identity")


RUBY_HARNESS = r'''
require "pathname"
require "fileutils"
require "json"
ROOT = Pathname(ARGV[1])
CALLS = []
TOOLS = JSON.parse((ROOT/"tools.json").read)
PROVISIONED = []
module OS
  def self.linux?; ARGV[2] == "linux"; end
end
module Utils
  def self.safe_popen_read(*args)
    CALLS << args.map(&:to_s)
    case args[1]
    when "scripts/homebrew_toolchains.py"
      (ROOT/"tools.json").read
    when "scripts/build-native-candidate.py"
      raise "toolchain not installed" unless PROVISIONED == [TOOLS.fetch("rust")]
      raise "nonisolated Cargo home" unless ENV["CARGO_HOME"] == "#{ROOT}/target/head-toolchains/cargo"
      raise "nonisolated Rustup home" unless ENV["RUSTUP_HOME"] == "#{ROOT}/target/head-toolchains/rustup"
      raise "wrong toolchain" unless ENV["RUSTUP_TOOLCHAIN"] == TOOLS.fetch("rust")
      raise "tools not ahead of ambient PATH" unless ENV["PATH"].start_with?("#{ROOT}/target/head-toolchains/bin:/formula/rustup/bin:")
      raise "missing Linux strip owner" if OS.linux? && ENV["ROTTWEILER_STRIP_BIN"] != "/formula/binutils/bin/strip"
      raise "unexpected Darwin strip owner" if !OS.linux? && ENV.key?("ROTTWEILER_STRIP_BIN")
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
    raise "candidate was not verified" unless CALLS.length == 3
    FileUtils.mkdir_p(to_s)
    paths.each { |path| FileUtils.cp(path, to_s) }
  end
  def install_symlink(path)
    FileUtils.mkdir_p(to_s)
    File.symlink(path, join(path.basename))
  end
end
class Resource
  attr_accessor :owner
  def initialize(_name, &block); instance_eval(&block); end
  def url(value); raise "wrong resource URL" unless value == TOOLS.fetch("bun").fetch("url"); end
  def sha256(value); raise "wrong checksum" unless value == TOOLS.fetch("bun").fetch("sha256"); end
  def stage
    directory = ROOT/"resource"
    directory.mkpath
    (directory/"bun").write("pinned Bun")
    Dir.chdir(directory) { yield }
  end
end
class Pathname
  def install(source)
    mkpath
    FileUtils.cp(source, to_s)
  end
end
class Formula
  def self.method_missing(*); end
  def self.test; end
  def self.on_linux; end
  def self.[](name); new(name); end
  def initialize(name = "rottweiler"); @name = name; end
  def opt_bin; Pathname("/formula/#{@name}/bin"); end
  def system(*args)
    expected = [Pathname("/formula/rustup/bin/rustup"), "toolchain", "install", TOOLS.fetch("rust"), "--no-self-update", "--profile", TOOLS.fetch("profile"), "--component", TOOLS.fetch("components").join(",")]
    raise "wrong Rust provisioning" unless args == expected
    PROVISIONED << TOOLS.fetch("rust")
  end
  def formula_opt_bin(name); Pathname("/formula/#{name}/bin"); end
  def with_env(values)
    previous = ENV.to_h
    values.each { |key, value| ENV[key.to_s] = value.to_s }
    yield
  ensure
    ENV.replace(previous)
  end
  def buildpath; ROOT; end
  def libexec; InstallPath.new("#{ROOT}/installed/libexec"); end
  def bin; InstallPath.new("#{ROOT}/installed/bin"); end
end
load ARGV[0]
Rottweiler.new.install
puts JSON.generate({calls: CALLS, private: Dir.children("#{ROOT}/installed/libexec"),
                    public: Dir.children("#{ROOT}/installed/bin")})
'''
