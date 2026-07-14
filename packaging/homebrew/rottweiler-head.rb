# typed: strict
# frozen_string_literal: true

# The complete Rottweiler engine and OpenTUI application bundle.
class Rottweiler < Formula
  desc "Provider-blind coding-agent harness with an OpenTUI frontend"
  homepage "https://github.com/Boredphilosopher96/Rottweiler"
  license "Apache-2.0"
  head "https://github.com/Boredphilosopher96/Rottweiler.git", branch: "main"
  depends_on "bun" => :build
  depends_on "rust" => :build
  on_linux do
    depends_on "binutils" => :build
  end
  # OpenTUI ships a private renderer with an @rpath install ID and no spare
  # Mach-O header padding. It is loaded by absolute sibling path at runtime.
  preserve_rpath

  def install
    # Keep the public binary's dependency features isolated from the private
    # Wasmtime helper. Selecting both roots together would unify rw-ext features.
    system "scripts/cargo-release.sh", "build", "--locked", "--release", "-p", "rw-cli"
    system "scripts/cargo-release.sh", "build", "--locked", "--release", "-p", "rw-wasm-host"
    system "bun", "install", "--cwd", "packages/tui", "--frozen-lockfile"
    if OS.linux?
      with_env(ROTTWEILER_STRIP_BIN: formula_opt_bin("binutils")/"strip") do
        system "bun", "run", "--cwd", "packages/tui", "build"
      end
    else
      system "bun", "run", "--cwd", "packages/tui", "build"
    end

    release_dir = Utils.safe_popen_read("scripts/cargo-release.sh", "artifact-dir").strip
    libexec.install "#{release_dir}/rw"
    libexec.install "#{release_dir}/rottweiler-wasm-host"
    libexec.install "packages/tui/dist/rottweiler-tui"
    native = OS.mac? ? "libopentui.dylib" : "libopentui.so"
    libexec.install "packages/tui/dist/#{native}"
    (bin/"rw").write_env_script libexec/"rw", ROTTWEILER_PACKAGE_MANAGER: "homebrew"
  end

  test do
    assert_match(/^rw \d+\.\d+\.\d+/, shell_output("#{bin}/rw --version"))
    assert_predicate libexec/"rottweiler-tui", :executable?
    assert_predicate libexec/"rottweiler-wasm-host", :executable?
    native = OS.mac? ? "libopentui.dylib" : "libopentui.so"
    assert_predicate libexec/native, :file?
    refute_path_exists bin/"rottweiler-tui"
    guidance = shell_output("#{bin}/rw upgrade 2>&1", 1)
    assert_match "managed by Homebrew", guidance
    assert_match "brew upgrade", guidance
  end
end
