{
  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";
    cargo-dyndrv = {
      url = "github:obsidiansystems/cargo-dyndrv";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      cargo-dyndrv,
      self,
    }:
    let
      inherit (nixpkgs) lib;
      makePkgs =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ cargo-dyndrv.overlays.default ];
        };
      forAllSystems = f: lib.genAttrs lib.systems.flakeExposed (system: f (makePkgs system));
    in
    {
      packages =
        let
          built =
            pkgs:
            pkgs.buildDynamicCrate {
              pname = "ffmpeg-example";
              version = "0.1.0";
              src = ./.;

              cargoLock.lockFile = ./Cargo.lock;

              extern = {
                "registry+https://github.com/rust-lang/crates.io-index#clang-sys@1.8.1" = {
                  env.LIBCLANG_PATH = "${lib.getLib pkgs.buildPackages.libclang}/lib";
                };

                "registry+https://github.com/rust-lang/crates.io-index#ffmpeg-sys-next@9.0.0" = {
                  path = [ "${lib.getBin pkgs.buildPackages.pkg-config}/bin" ];
                  env = {
                    PKG_CONFIG_PATH = "${lib.getDev pkgs.ffmpeg-headless}/lib/pkgconfig";
                    PKG_CONFIG = lib.getExe pkgs.buildPackages.pkg-config;
                    BINDGEN_EXTRA_CLANG_ARGS = "-I${pkgs.stdenv.cc.libc.dev}/include";
                    LIBCLANG_PATH = "${lib.getLib pkgs.buildPackages.libclang}/lib";
                  };
                };
              };

              outputs = [ "ffmpeg-example" ];
            };
        in
        forAllSystems (pkgs: {
          default = built pkgs;
          cross = built pkgs.pkgsCross.aarch64-multiplatform;
        });
      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
