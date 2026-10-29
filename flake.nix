{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    nixpkgs,
    rust-overlay,
    flake-utils,
    crane,
    self,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      overlays = [(import rust-overlay)];
      pkgs = import nixpkgs {
        inherit system overlays;
      };
      lib = pkgs.lib;
      gitTag = self.shortRev or "dev";
      craneLib = (crane.mkLib pkgs).overrideToolchain (
        p:
          p.rust-bin.stable.latest.default.override {
            targets = ["x86_64-unknown-linux-gnu" "wasm32-unknown-unknown"];
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          }
      );

      rustNightly = pkgs.rust-bin.selectLatestNightlyWith (t:
        t.default.override {
          targets = ["wasm32-unknown-unknown"];
          extensions = [
            "rust-src"
            "rust-analyzer"
          ];
        });
      rustNightlySrc = pkgs.rust-bin.selectLatestNightlyWith (t: t.rust-src);
      devCraneLib = craneLib.overrideToolchain rustNightly;

      nightlyShims = pkgs.symlinkJoin {
        name = "nightly-shims";
        paths =
          map (tool:
            pkgs.writeShellScriptBin tool ''
              [ "$1" = +nightly ] && shift
              exec ${rustNightly}/bin/${tool} "$@"
            '') ["cargo" "rustc"]
          ++ [(pkgs.writeShellScriptBin "rustup" "echo 'rust-src (installed)'")];
      };

      workerBuild = pkgs.rustPlatform.buildRustPackage (finalAttrs: {
        pname = "worker-build";
        version = "0.8.7";
        src = pkgs.fetchFromGitHub {
          owner = "cloudflare";
          repo = "workers-rs";
          tag = "v${finalAttrs.version}";
          hash = "sha256-ZZFyZeu/PGwUatOWG/SPI7y+XyiuUyHO7L5y0qtJl70=";
          fetchSubmodules = true;
        };
        cargoHash = "sha256-AHTBKz8jwUaQpC03oH15NxtBQYO5PrvUVLwkz4+L5K8=";
        buildAndTestSubdir = "worker-build";
        nativeBuildInputs = [pkgs.pkg-config];
        buildInputs = [pkgs.openssl];
        doCheck = false;
      });
      src = lib.cleanSourceWith {
        src = ./.;
        # json files are not in cranes default fitler
        filter = path: type: lib.hasSuffix ".json" path || craneLib.filterCargoSources path type;
      };

      wasmBindgenCli = pkgs.buildWasmBindgenCli rec {
        src = pkgs.fetchCrate {
          pname = "wasm-bindgen-cli";
          version = "0.2.128";
          hash = "sha256-a7lcXJnnZkYReja+iUO7NqqrWyv3toxnUgQb8s4IS5s=";
        };
        cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
          inherit src;
          inherit (src) pname version;
          hash = "sha256-R1Tas33Ursy8kqsxguAkG0ZhNed2n5uFTAhw1l2qlLY=";
        };
      };

      workerVendorDir = devCraneLib.vendorMultipleCargoDeps {
        cargoLockList = [./Cargo.lock];
        cargoLockContentsList = [
          (builtins.readFile "${rustNightlySrc}/lib/rustlib/src/rust/library/Cargo.lock")
        ];
      };

      mkWorker = app: unwind: let
        workerArgs = {
          pname = "${app}-worker";
          inherit src;
          cargoVendorDir = workerVendorDir;
          nativeBuildInputs = [nightlyShims];
          WASM_BINDGEN_USE_JS_SYS = "1";
        };
        buildStd = lib.optionalString unwind " -Z build-std=std,panic_unwind";
        cargoArtifacts = devCraneLib.buildDepsOnly (workerArgs
          // {
            buildPhaseCargoCommand = "cargo build --release --lib -p ${app} --target wasm32-unknown-unknown${buildStd}";
            doCheck = false;
          }
          // lib.optionalAttrs unwind {RUSTFLAGS = "-Cpanic=unwind";});
      in
        devCraneLib.mkCargoDerivation (workerArgs
          // {
            version = gitTag;
            inherit cargoArtifacts;
            doInstallCargoArtifacts = false;
            nativeBuildInputs = workerArgs.nativeBuildInputs ++ [workerBuild];
            ESBUILD_BIN = "${pkgs.esbuild}/bin/esbuild";
            WASM_OPT_BIN = "${pkgs.binaryen}/bin/wasm-opt";
            WASM_BINDGEN_BIN = "${wasmBindgenCli}/bin/wasm-bindgen";
            buildPhaseCargoCommand = "(cd apps/${app} && worker-build --release${lib.optionalString unwind " --panic-unwind"})";
            installPhaseCommand = "cp -r apps/${app}/build $out";
          });
      abembedWorker = mkWorker "abembed" true;
      providerWorker = mkWorker "provider" true;
      proxyWorker = mkWorker "proxy" false;

      commonRustArgs = {
        inherit src;
        strictDeps = true;

        nativeBuildInputs = with pkgs; [
          cmake
          pkg-config
        ];

        PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
      };

      cargoArtifacts = craneLib.buildDepsOnly commonRustArgs;

      individualCrateArgs =
        commonRustArgs
        // {
          inherit cargoArtifacts;
          inherit (craneLib.crateNameFromCargoToml {inherit src;}) version;
          # we disable tests since we'll run them all via cargo-nextest
          doCheck = false;
        };

      apiPackages = import ./apps/api/nix/packages.nix {
        inherit pkgs craneLib individualCrateArgs gitTag;
      };

      inherit (apiPackages) api apiDockerImage;

      gifServicePackages = import ./apps/gif-service/nix/packages.nix {
        inherit pkgs craneLib individualCrateArgs gitTag;
      };

      inherit (gifServicePackages) gifService gifServiceDockerImage;

      embedImageServiceFileset = lib.fileset.unions [
        ./pnpm-lock.yaml
        ./pnpm-workspace.yaml
        ./package.json
        ./apps/embed-image-service/package.json
        ./apps/embed-image-service/tsconfig.json
        ./apps/embed-image-service/src
        ./packages/embed-image/package.json
        ./packages/embed-image/tsconfig.json
        ./packages/embed-image/vite.config.ts
        ./packages/embed-image/src
        ./packages/config/tsconfig
      ];

      embedImageServiceSrc = lib.fileset.toSource {
        root = ./.;
        fileset = embedImageServiceFileset;
      };

      embedImageServicePnpmDeps = pkgs.fetchPnpmDeps {
        pname = "embed-image-service";
        version = "0.1.0";
        src = embedImageServiceSrc;
        fetcherVersion = 3;
        hash = "sha256-llxysmPg44mu87bM4/UUHlIHbayPXpIBm4DDNWWIDiQ=";
      };

      embedImageServicePackages = import ./apps/embed-image-service/nix/packages.nix {
        inherit pkgs embedImageServiceSrc embedImageServicePnpmDeps gitTag;
      };

      inherit (embedImageServicePackages) embedImageService embedImageServiceDockerImage;
    in {
      checks = {
        inherit api embedImageService gifService abembedWorker providerWorker proxyWorker;

        workspace-clippy = craneLib.cargoClippy (
          commonRustArgs
          // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          }
        );
        workspace-fmt = craneLib.cargoFmt {
          inherit src;
        };

        workspace-nextest = craneLib.cargoNextest (
          commonRustArgs
          // {
            inherit cargoArtifacts;
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
            partitions = 1;
            partitionType = "count";
            cargoNextestPartitionsExtraArgs = "--no-tests=pass";
          }
        );
      };
      devShells.default = with pkgs;
        devCraneLib.devShell {
          checks = self.checks.${system};

          packages = [
            nodejs_25
            pnpm_10
            pkg-config
            openssl
            just
            mprocs
            kubernetes-helm
            ffmpeg-headless
            gifski
            wrangler
            workerBuild
          ];

          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";

          shellHook = ''
            export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig";
            export PATH="${nightlyShims}/bin:$PATH"
            export PATH="$PWD/node_modules/.bin/:$PATH"
          '';
        };

      packages = {
        inherit api apiDockerImage embedImageService embedImageServiceDockerImage gifService gifServiceDockerImage abembedWorker providerWorker proxyWorker;
      };
    });
}
