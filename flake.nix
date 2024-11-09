{
  description = "Build a cargo project";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-analyzer-src.follows = "";
    };

    flake-utils.url = "github:numtide/flake-utils";

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };

    # nil-lsp.url = "github:oxalica/nil";
  };

  outputs = { self, nixpkgs, crane, fenix, flake-utils, advisory-db, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
#        pkgs = nixpkgs.legacyPackages.${system};
        pkgs = import nixpkgs { inherit system; config = {
                allowUnsupportedSystem = true;
                allowUnfree = true;
            };
        };

#        rustToolchainForPkgs = p: {
#            rustc = p.rustc;
#            cargo = p.cargo;
#            rustfmt = p.rustfmt;
#            clippy = p.clippy;
#        };
#        rustToolchain = rustToolchainForPkgs pkgs;

        inherit (pkgs) lib;

#        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchainForPkgs;
#        craneLib = crane.mkLib pkgs;
        craneLib = (crane.mkLib pkgs).overrideToolchain
          (fenix.packages.${system}.stable.toolchain);
        src = craneLib.cleanCargoSource ./.;

        # Common arguments can be set here to avoid repeating them later
        commonArgs = {
          inherit src;
          strictDeps = true;

          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.rustPlatform.bindgenHook
            pkgs.gcc
          ];

          buildInputs = [
            pkgs.openssl
          ] ++ lib.optionals pkgs.stdenv.isDarwin [
            # Additional darwin specific inputs can be set here
            # pkgs.libiconv
          ];

          # Additional environment variables can be set directly
          # MY_CUSTOM_VAR = "some value";
        };

        craneLibLLvmTools = craneLib.overrideToolchain
          (fenix.packages.${system}.stable.toolchain);

        # Build *just* the cargo dependencies, so we can reuse
        # all of that work (e.g. via cachix) when running in CI
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Build the actual crate itself, reusing the dependency
        # artifacts from above.
        my-crate = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
        });

        rocmDeps = with pkgs.rocmPackages; [
#            rocm-core clr rccl miopen rocrand rocblas
#            rocsparse hipsparse rocthrust rocprim hipcub roctracer
#            rocfft rocsolver hipfft hipsolver hipblas
#            rocminfo rocm-thunk rocm-comgr rocm-device-libs
#            rocm-runtime clr.icd hipify
            rocm-core
            clr
            rocm-runtime
            hipblas
            rocblas
        ];

        rocmtoolkit_joined = pkgs.symlinkJoin {
          name = "rocm-merged";


          paths = rocmDeps;


          # Fix `setuptools` not being found
          postBuild = ''
            rm -rf $out/nix-support
          '';
        };
      in
      {
        # checks = {
        #   # Build the crate as part of `nix flake check` for convenience
        #   inherit my-crate;

        #   # Run clippy (and deny all warnings) on the crate source,
        #   # again, reusing the dependency artifacts from above.
        #   #
        #   # Note that this is done as a separate derivation so that
        #   # we can block the CI if there are issues here, but not
        #   # prevent downstream consumers from building our crate by itself.
        #   my-crate-clippy = craneLib.cargoClippy (commonArgs // {
        #     inherit cargoArtifacts;
        #     cargoClippyExtraArgs = "--all-targets -- --deny warnings";
        #   });

        #   my-crate-doc = craneLib.cargoDoc (commonArgs // {
        #     inherit cargoArtifacts;
        #   });

        #   # Check formatting
        #   my-crate-fmt = craneLib.cargoFmt {
        #     inherit src;
        #   };

        #   my-crate-toml-fmt = craneLib.taploFmt {
        #     src = pkgs.lib.sources.sourceFilesBySuffices src [ ".toml" ];
        #     # taplo arguments can be further customized below as needed
        #     # taploExtraArgs = "--config ./taplo.toml";
        #   };

        #   # Audit dependencies
        #   my-crate-audit = craneLib.cargoAudit {
        #     inherit src advisory-db;
        #   };

        #   # Audit licenses
        #   # my-crate-deny = craneLib.cargoDeny {
        #   #   inherit src;
        #   # };

        #   # Run tests with cargo-nextest
        #   # Consider setting `doCheck = false` on `my-crate` if you do not want
        #   # the tests to run twice
        #   my-crate-nextest = craneLib.cargoNextest (commonArgs // {
        #     inherit cargoArtifacts;
        #     partitions = 1;
        #     partitionType = "count";
        #   });
        # };

        # packages = {
        #   default = my-crate;
        # } // lib.optionalAttrs (!pkgs.stdenv.isDarwin) {
        #   my-crate-llvm-coverage = craneLibLLvmTools.cargoLlvmCov (commonArgs // {
        #     inherit cargoArtifacts;
        #   });
        # };

        # apps.default = flake-utils.lib.mkApp {
        #   drv = my-crate;
        # };

#        devShells.default = craneLib.devShell {
#          # Inherit inputs from checks.
#          # checks = self.checks.${system};
#
#          # Additional dev-shell environment variables can be set directly
#          # MY_CUSTOM_DEVELOPMENT_VAR = "something else";
#
#          # Extra inputs can be added here; cargo and rustc are provided by default.
#          packages = [
#            # pkgs.ripgrep
#            # pkgs.rust-analyzer
##            fenix.packages.${system}.stable.toolchain
#            # nil-lsp.packages.x86_64-linux.nil
##            pkgs.clippy
#          ];
#        };
         devShells.default = pkgs.mkShell {
           shellHook = ''
            export ROCM_PATH=${rocmtoolkit_joined}
            export HIP_PATH=${rocmtoolkit_joined}
            export ROCM_SOURCE_DIR=${rocmtoolkit_joined}
#            export CMAKE_CXX_FLAGS="-I${rocmtoolkit_joined}/include -I${rocmtoolkit_joined}/include/rocblas"
           '';
           buildInputs = rocmDeps ++ (with pkgs; [
             rustup
             pkg-config
             openssl
             openblas
             cargo-binutils
             libxml2
             rustPlatform.bindgenHook
             clang
             cudatoolkit
             cmake
             ffmpeg
#             pkgsCross.mingwW64.gcc
#             pkgsCross.mingwW64.stdenv.cc
           ]);
         };
      });
}

