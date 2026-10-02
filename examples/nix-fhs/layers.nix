# The FHS layers under test, built from the real NixOS nix-ld module.
#
# Each layer is what `programs.nix-ld` would put on a NixOS system with a given
# `libraries` list: `ldso` is the shim NixOS installs at /lib64/ld-linux-x86-64.so.2,
# and `sw/share/nix-ld/lib` is the library tree NixOS links into
# /run/current-system/sw.
let
  rev = "c59305bab2065cfecc4944690d9eedbb56f3a9fa"; # nixos-unstable, 2026-10-02
  nixpkgs = builtins.getFlake "github:NixOS/nixpkgs/${rev}";
  pkgs = nixpkgs.legacyPackages.x86_64-linux;

  layer = name: libraries:
    let
      cfg = (import "${nixpkgs}/nixos/lib/eval-config.nix" {
        system = "x86_64-linux";
        modules = [{
          boot.isContainer = true;
          system.stateVersion = "26.05";
          programs.nix-ld.enable = true;
          programs.nix-ld.libraries = libraries;
        }];
      }).config;
      libs = builtins.head (builtins.filter (p: (p.name or "") == "ld-library-path")
        cfg.environment.systemPackages);
    in
    pkgs.runCommand "fhs-layer-${name}" { } ''
      mkdir -p $out
      ln -s ${cfg.environment.ldso} $out/ldso
      ln -s ${libs} $out/sw
    '';

  legacySonameShims = pkgs.runCommand "legacy-soname-shims" { } ''
    mkdir -p $out/lib
    ln -s ${pkgs.libxml2.out}/lib/libxml2.so $out/lib/libxml2.so.2
  '';

  # A base FHS library set. The module defaults are deliberately NOT restated, to test
  # whether the module merges them in.
  base = with pkgs; [
    stdenv.cc.cc.lib libffi libxcrypt elfutils libunwind
    libxcrypt-legacy
    ncurses readline sqlite expat pcre2 icu
    lz4 brotli snappy
    legacySonameShims
  ];

  desktop = with pkgs; [
    glib gtk3 cairo pango atk gdk-pixbuf at-spi2-atk at-spi2-core
    nss nspr dbus fontconfig freetype
    libGL libdrm libxkbcommon mesa libgbm vulkan-loader
    libx11 libxcomposite libxdamage libxext libxfixes libxrandr libxrender libxi libxtst
    libxscrnsaver libxcb libxcursor libxshmfence
    webkitgtk_4_1 libsoup_3 alsa-lib libpulseaudio cups
  ];
in
pkgs.linkFarm "fhs-layers" [
  { name = "stock"; path = layer "stock" [ ]; }
  { name = "base"; path = layer "base" base; }
  { name = "desktop"; path = layer "desktop" (base ++ desktop); }
  # Candidate fixes for gaps the corpus found (see corpus.sh "fix:" tests).
  { name = "tzdata"; path = pkgs.tzdata; }
  { name = "glibc-bin"; path = pkgs.glibc.bin; }
]
