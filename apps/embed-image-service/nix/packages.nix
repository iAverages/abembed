{
  pkgs,
  embedImageServiceSrc,
  embedImageServicePnpmDeps,
  gitTag,
}: let
  embedImageService = pkgs.stdenvNoCC.mkDerivation {
    pname = "embed-image-service";
    version = "0.1.0";

    src = embedImageServiceSrc;
    pnpmDeps = embedImageServicePnpmDeps;

    nativeBuildInputs = [
      pkgs.nodejs_26
      pkgs.pnpm_12
      pkgs.pnpmConfigHook
    ];

    buildPhase = ''
      runHook preBuild

      pnpm install --frozen-lockfile --offline --filter @spqtify/embed-image-service... --filter @spqtify/tsconfig

      pnpm --filter @spqtify/embed-image build
      pnpm --filter @spqtify/embed-image-service build

      runtime_dir="$TMPDIR/runtime"
      pnpm --filter @spqtify/embed-image-service deploy --prod --offline --config.inject-workspace-packages=true "$runtime_dir"

      mkdir -p "$runtime_dir/dist"
      ls apps/embed-image-service
      cp -R "apps/embed-image-service/dist"/. "$runtime_dir/dist/"

      rm -rf "$runtime_dir/src"
      rm -rf "$runtime_dir/.turbo"
      rm -rf "$runtime_dir/node_modules/.bin"
      rm -f "$runtime_dir/tsconfig.json"
      rm -f "$runtime_dir/tsconfig.tsbuildinfo"

      rm -rf "$runtime_dir/node_modules/@spqtify/embed-image/src"
      rm -f "$runtime_dir/node_modules/@spqtify/embed-image/tsconfig.json"
      rm -f "$runtime_dir/node_modules/@spqtify/embed-image/vite.config.ts"
      rm -f "$runtime_dir/node_modules/@spqtify/embed-image/tsconfig.tsbuildinfo"

      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      mkdir -p "$out"
      cp -R "$TMPDIR/runtime"/. "$out"/
      runHook postInstall
    '';
  };

  embedImageServiceDockerImage = pkgs.dockerTools.buildLayeredImage {
    name = "spqtify-embed-image-service";
    tag = gitTag;

    contents = [
      pkgs.nodejs_26
      embedImageService
      pkgs.cacert
    ];

    config = {
      Cmd = [
        "${pkgs.nodejs_26}/bin/node"
        "${embedImageService}/dist/index.js"
      ];
      ExposedPorts = {
        "3001/tcp" = {};
      };
      Env = [
        "NODE_ENV=production"
        "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
      ];
    };
  };
in {
  inherit embedImageService embedImageServiceDockerImage;
}
