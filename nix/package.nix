# The whole levelup workspace as a single package: ten binaries out of one
# cargo invocation.
#
# One derivation rather than ten was deliberate. The crates share a workspace,
# a lockfile and two internal libraries (levelup-core, levelup-tui), so ten
# separate derivations would each vendor and compile the same dependency graph
# -- ten times the build, bought with the ability to install munin without
# bragi. Not worth it.
#
# Kept callPackage-able rather than inlined in flake.nix so the overlay and any
# bare `pkgs.callPackage ./nix/package.nix { }` get the same derivation from
# the same file.
{
  lib,
  stdenv,
  rustPlatform,
  installShellFiles,
  pkg-config,
  dbus,
  pipewire,
}:

rustPlatform.buildRustPackage {
  pname = "levelup";
  version = "0.1.0";

  # An explicit allow-list, not `../.`. target/ is 544 MB against ~1 MB of
  # source: inside a flake git already hides it, but `pkgs.callPackage` against
  # a plain path -- which the overlay makes possible -- has no such protection.
  # Leaving flake.nix, README.org, justfile and dist/ out also means editing
  # them never invalidates a multi-minute Rust build.
  #
  # lib.fileset over lib.cleanSourceWith: an allow-list of paths cannot
  # silently start matching something new. lib.fileset.gitTracked was rejected
  # -- it needs a .git directory, absent once this flake is an input.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../levelup-core
      ../levelup-tui
      ../hugin
      ../munin
      ../mimir
      ../sleipnir
      ../valkyrie
      ../heimdall
      ../gorm
      ../bragi
    ];
  };

  # The committed lockfile is the source of truth, so there is no cargoHash to
  # bump. No outputHashes either: every source in Cargo.lock is crates.io, and
  # that attribute exists only for git dependencies.
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    # libdbus-sys (gorm, via bluer) and libspa-sys/pipewire-sys (bragi) find
    # their C libraries through pkg-config.
    pkg-config
    # pipewire-sys and libspa-sys generate their bindings with bindgen; this
    # hook is what sets LIBCLANG_PATH and BINDGEN_EXTRA_CLANG_ARGS for them.
    rustPlatform.bindgenHook
    installShellFiles
  ];

  buildInputs = [
    # gorm -> bluer(bluetoothd) -> dbus -> libdbus-sys. A real link-time dep:
    # libdbus-1.so ends up in the runtime closure.
    dbus
    # bragi -> pipewire + libspa. The dev output carries both
    # libpipewire-0.3.pc and libspa-0.2.pc, and buildInputs already selects
    # dev, so naming the package once is enough.
    pipewire
  ];

  # Deliberately absent:
  #   * git       -- the eight build.rs files shell out to it for GIT_COMMIT
  #                  and fall back to "unknown" when it (or .git) is missing,
  #                  which is always the case here. `--version` therefore
  #                  prints "0.1.0 (unknown)"; the store path is the real
  #                  build identity.
  #   * sqlite    -- rusqlite uses features = ["bundled"], so libsqlite3-sys
  #                  compiles vendored SQLite with cc; stdenv's compiler is all
  #                  it needs.
  #   * wayland   -- wayland-client is used without use_system_lib, so the
  #                  pure-Rust backend is in play and nothing links
  #                  libwayland-client.
  #   * oniguruma -- syntect is on default-fancy (pure-Rust regex).

  # No cargoBuildFlags: the root Cargo.toml is a virtual manifest with no
  # default-members, so a bare `cargo build` already builds all ten members.
  #
  # doCheck is left at its default true: the 97 inline tests parse string
  # fixtures or write under $TMPDIR -- no network, no display, no tty.

  postInstall = lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
    # Completions come from running the binaries that were just built, which
    # only works when the build machine can execute them. That is always true
    # on x86_64-linux; the guard is one line and keeps a hypothetical cross
    # build from dying here instead of shipping without completions.
    #
    # The SHELL argument is optional in the CLI and falls back to $SHELL, which
    # is unset in the sandbox -- so it is always passed explicitly.
    for tool in hugin munin mimir sleipnir valkyrie heimdall gorm bragi; do
      installShellCompletion --cmd "$tool" \
        --bash <("$out/bin/$tool" completions bash) \
        --fish <("$out/bin/$tool" completions fish) \
        --zsh <("$out/bin/$tool" completions zsh)
    done

    # hugind is the exception: a --generate-completions flag rather than a
    # subcommand, because a daemon has no subcommands to hang one off.
    # munind has neither and therefore gets none.
    installShellCompletion --cmd hugind \
      --bash <($out/bin/hugind --generate-completions bash) \
      --fish <($out/bin/hugind --generate-completions fish) \
      --zsh <($out/bin/hugind --generate-completions zsh)
  '';

  meta = {
    description = "Wayland/Linux desktop tools: clipboard, shell history, status bar, navigation, processes, LAN, bluetooth, audio";
    homepage = "https://github.com/ast/levelup";
    license = lib.licenses.mit;
    # Wayland (hugin), BlueZ over D-Bus (gorm) and PipeWire (bragi) are
    # Linux-only, and several tools parse /proc directly.
    platforms = lib.platforms.linux;
    # Arbitrary among ten binaries, but `nix run` and lib.getExe need one name.
    mainProgram = "hugin";
  };
}
