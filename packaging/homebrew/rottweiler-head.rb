# typed: strict
# frozen_string_literal: true

# The complete Rottweiler engine and OpenTUI application bundle.
class Rottweiler < Formula
  desc "Provider-blind coding-agent harness with an OpenTUI frontend"
  homepage "https://github.com/Boredphilosopher96/Rottweiler"
  license "Apache-2.0"
  head "https://github.com/Boredphilosopher96/Rottweiler.git", branch: "main"
  depends_on "rustup" => :build
  depends_on "python@3.14" => :build
  on_linux do
    depends_on "binutils" => :build
  end
  # OpenTUI ships a private renderer with an @rpath install ID and no spare
  # Mach-O header padding. It is loaded by absolute sibling path at runtime.
  preserve_rpath

  def install
    python = Formula["python@3.14"].opt_bin/"python3.14"
    with_native_toolchains(python) do
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
    end
    bin.install_symlink libexec/"rw"
  end

  def with_native_toolchains(python)
    tools = JSON.parse(Utils.safe_popen_read(python, "scripts/homebrew_toolchains.py"))
    directory = buildpath/"target/head-toolchains"
    bun = tools.fetch("bun")
    resource = Resource.new("rottweiler-head-bun") do
      url bun.fetch("url")
      sha256 bun.fetch("sha256")
    end
    resource.owner = self
    resource.stage { (directory/"bin").install "bun" }
    zig = tools.fetch("zig")
    zig_resource = Resource.new("rottweiler-head-zig") do
      url zig.fetch("url")
      sha256 zig.fetch("sha256")
    end
    zig_resource.owner = self
    zig_resource.fetch
    rustup_bin = Formula["rustup"].opt_bin
    with_env(
      PATH: "#{directory}/bin:#{rustup_bin}:#{ENV.fetch('PATH')}",
      ROTTWEILER_ZIG_ARCHIVE: zig_resource.cached_download,
      CARGO_HOME: directory/"cargo", RUSTUP_HOME: directory/"rustup",
      RUSTUP_TOOLCHAIN: tools.fetch("rust"), RUSTUP_AUTO_INSTALL: "0",
      RUSTUP_DIST_SERVER: "https://static.rust-lang.org", RUSTC: rustup_bin/"rustc"
    ) do
      system rustup_bin/"rustup", "toolchain", "install", tools.fetch("rust"),
             "--no-self-update", "--profile", tools.fetch("profile"),
             "--component", tools.fetch("components").join(",")
      yield
    end
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
