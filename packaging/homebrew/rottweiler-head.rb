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
  depends_on "python@3.14" => :build
  on_linux do
    depends_on "binutils" => :build
  end
  # OpenTUI ships a private renderer with an @rpath install ID and no spare
  # Mach-O header padding. It is loaded by absolute sibling path at runtime.
  preserve_rpath

  def install
    python = Formula["python@3.14"].opt_bin/"python3.14"
    build_candidate = lambda do
      # The builder checks the exact source toolchains, platform, and product limits.
      Utils.safe_popen_read(
        python, "scripts/build-native-candidate.py",
        "--output", buildpath/"dist/native-candidates", "--target-dir", buildpath/"target"
      ).strip
    end
    candidate = if OS.linux?
      with_env(ROTTWEILER_STRIP_BIN: formula_opt_bin("binutils")/"strip", &build_candidate)
    else
      build_candidate.call
    end
    engine = Pathname(Utils.safe_popen_read(
      python, "scripts/native_candidate.py", "path", candidate, "engine"
    ).strip)
    libexec.install Dir[(engine.dirname/"*").to_s]
    bin.install_symlink libexec/"rw"
  end

  test do
    assert_match(/^rw \d+\.\d+\.\d+/, shell_output("#{bin}/rw --version"))
    assert_predicate libexec/"rottweiler-js-host", :executable?
    assert_predicate libexec/"rottweiler-wasm-host", :executable?
    native = OS.mac? ? "libopentui.dylib" : "libopentui.so"
    assert_predicate libexec/native, :file?
    refute_path_exists bin/"rottweiler-js-host"
    guidance = shell_output("#{bin}/rw upgrade 2>&1", 1)
    assert_match "managed by Homebrew", guidance
    assert_match "brew upgrade", guidance
  end
end
