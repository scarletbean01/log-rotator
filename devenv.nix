{ pkgs, lib, config, inputs, ... }:

{
  # ---------------------------------------------------------------------------
  # Toolchain
  # ---------------------------------------------------------------------------
  languages.rust = {
    enable = true;
    channel = "stable";
    targets = [ "x86_64-unknown-linux-musl" ];
    mold.enable = pkgs.stdenv.isLinux;
  };

  packages = [
    pkgs.git
    pkgs.esbuild
    pkgs.zig
    pkgs.cargo-watch
  ];

  # Cross-linker wrapper: zig provides the musl C runtime + crt for the
  # pure-Rust dependency tree (no openssl, no C build scripts).
  scripts.zig-cc-musl.exec = ''
    exec ${pkgs.zig}/bin/zig cc -target x86_64-linux-musl "$@"
  '';

  # ---------------------------------------------------------------------------
  # Task DAG
  # ---------------------------------------------------------------------------
  tasks = {
    "ui:build" = {
      exec = ''
        mkdir -p ui/dist
        esbuild ui/app.ts --bundle --minify --target=es2020 --outfile=ui/dist/app.js
        cp ui/index.html ui/style.css ui/dist/
      '';
      execIfModified = [ "ui/*.ts" "ui/*.html" "ui/*.css" ];
    };

    "logsidecar:build".exec = "cargo build";
    "logsidecar:build".after = [ "ui:build" ];

    "logsidecar:test".exec = "cargo test";
    "logsidecar:test".after = [ "ui:build" ];

    "logsidecar:clippy".exec = "cargo clippy --all-targets -- -D warnings";

    "logsidecar:fmt".exec = "cargo fmt --check";

    "logsidecar:check".exec = "true";
    "logsidecar:check".after = [
      "logsidecar:fmt"
      "logsidecar:clippy"
      "logsidecar:test"
    ];

    # mold.enable exports RUSTFLAGS, which makes cargo ignore ALL config-file
    # rustflags (including .cargo/config.toml). Re-append the flag here or
    # rustc links its bundled musl CRT and collides with zig's crt1.o.
    "logsidecar:release".exec = ''
      export RUSTFLAGS="$RUSTFLAGS -C link-self-contained=no"
      cargo build --release --target x86_64-unknown-linux-musl
    '';
    "logsidecar:release".after = [ "ui:build" "logsidecar:check" ];
  };

  # ---------------------------------------------------------------------------
  # Dev process
  # ---------------------------------------------------------------------------
  processes.dev.exec = "cargo watch -x 'run -- --root ./testdata/logs'";

  # ---------------------------------------------------------------------------
  # Tests
  # ---------------------------------------------------------------------------
  enterTest = ''
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test
  '';

  # ---------------------------------------------------------------------------
  # Shell
  # ---------------------------------------------------------------------------
  enterShell = ''
    echo "log-sidecar: devenv tasks run logsidecar:check"
  '';
}
