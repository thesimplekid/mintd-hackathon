{
  description = "Cashu Development Kit";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-23.11";

    flakebox = {
      url = "github:rustshop/flakebox";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flakebox, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { system = system; };
        lib = pkgs.lib;
        flakeboxLib = flakebox.lib.${system} { };
        rustSrc = flakeboxLib.filterSubPaths {
          root = builtins.path {
            name = "cdk";
            path = ./.;
          };
          paths = [ "crates/cashu" "crates/cashu-sdk" ];
        };

        toolchainArgs = let llvmPackages = pkgs.llvmPackages_11;
        in {
          extraRustFlags = "--cfg tokio_unstable";

          components = [ "rustc" "cargo" "clippy" "rust-analyzer" "rust-src" ];

          args = {
            nativeBuildInputs =
              [ pkgs.wasm-bindgen-cli pkgs.geckodriver pkgs.wasm-pack ]
              ++ lib.optionals (!pkgs.stdenv.isDarwin) [ pkgs.firefox ];
          };
        } // lib.optionalAttrs pkgs.stdenv.isDarwin {
          # on Darwin newest stdenv doesn't seem to work
          # linking rocksdb
          stdenv = pkgs.clang11Stdenv;
          clang = llvmPackages.clang;
          libclang = llvmPackages.libclang.lib;
          clang-unwrapped = llvmPackages.clang-unwrapped;
        };

        targetsStd = flakeboxLib.mkStdTargets { };
        toolchainsStd = flakeboxLib.mkStdFenixToolchains toolchainArgs;

        toolchainNative = flakeboxLib.mkFenixToolchain {
          targets = (pkgs.lib.getAttrs [ "default" "wasm32-unknown" ] targetsStd);
        };

        commonArgs = {
          buildInputs = [ pkgs.openssl ] ++ lib.optionals pkgs.stdenv.isDarwin
            [ pkgs.darwin.apple_sdk.frameworks.SystemConfiguration ];
          nativeBuildInputs = [ pkgs.pkg-config ];
        };
        outputs = (flakeboxLib.craneMultiBuild { toolchains = toolchainsStd; })
          (craneLib':
            let
              craneLib = (craneLib'.overrideArgs {
                pname = "flexbox-multibuild";
                src = rustSrc;
              }).overrideArgs commonArgs;
            in rec {
              workspaceDeps = craneLib.buildWorkspaceDepsOnly { };
              workspaceBuild =
                craneLib.buildWorkspace { cargoArtifacts = workspaceDeps; };
            });
      in {
        devShells = flakeboxLib.mkShells {
          toolchain = toolchainNative;
          packages = [ ];
          nativeBuildInputs = with pkgs; [ wasm-pack sqlx-cli ];
          shellHook = ''
            export RUSTFLAGS="--cfg tokio_unstable"
            export RUSTDOCFLAGS="--cfg tokio_unstable"
            export RUST_LOG="info"
          '';
        
        };
      });
}
