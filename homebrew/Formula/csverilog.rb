class Csverilog < Formula
  desc "IEEE 1364 Verilog parser, optimizer, and VCD simulator (Circuit Scope CLI)"
  homepage "https://github.com/um-mepel/circuit-scope-verilog"
  url "https://github.com/um-mepel/circuit-scope-verilog/archive/refs/tags/v0.3.0.tar.gz"
  # Regenerate after each release with:
  #   curl -L https://github.com/um-mepel/circuit-scope-verilog/archive/refs/tags/vX.Y.Z.tar.gz | shasum -a 256
  sha256 "e85e3aabdbf199b4491aa287d4ce33cd7b8a618fd7cd8a205dc09c2770972120"
  license "MIT"
  head "https://github.com/um-mepel/circuit-scope-verilog.git", branch: "main"

  depends_on "rust" => :build

  def install
    # The `verilog_core` crate is a self-contained Cargo package with its own
    # lockfile at src-tauri/verilog-core/Cargo.lock; build the `csverilog` bin
    # from there with `--locked` for reproducibility.
    cd "src-tauri/verilog-core" do
      system "cargo", "install", *std_cargo_args(path: ".")
    end
  end

  test do
    # `csverilog` prints a usage banner to stderr and exits with status 1 when
    # invoked without arguments; verify both.
    output = shell_output("#{bin}/csverilog 2>&1", 1)
    assert_match "Usage: csverilog", output

    # Compile a trivial combinational module end-to-end and confirm a VCD is produced.
    (testpath/"top.v").write <<~EOS
      module top;
        reg a, b;
        wire y;
        assign y = a & b;
        initial begin
          a = 0; b = 0;
          #1 a = 1;
          #1 b = 1;
          #1 ;
        end
      endmodule
    EOS

    # Usage: csverilog [options] <output> [--explicit <file.v> ...]
    system bin/"csverilog", "--cycles", "4", "out", "--explicit", "top.v"
    assert_predicate testpath/"out.vcd", :exist?
  end
end
