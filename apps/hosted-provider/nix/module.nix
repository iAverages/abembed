self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.hosted-provider;
in {
  options.services.hosted-provider = {
    enable = lib.mkEnableOption "the abembed hosted provider";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.hostedProvider;
      defaultText = lib.literalExpression "abembed.packages.\${system}.hostedProvider";
      description = "The hosted-provider package to run.";
    };

    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:3003";
      description = ''
        Address to listen on. Workers only fetch https hostnames, so put a reverse proxy or
        tunnel in front of it.
      '';
    };

    interface = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "wg0";
      description = ''
        Network interface outgoing requests are sent through, to route them over a tunnel. Needs a
        route for connections bound to it, requests fail rather than bypass it when it is down.
      '';
    };

    environmentFile = lib.mkOption {
      type = lib.types.str;
      example = "/run/secrets/hosted-provider.env";
      description = ''
        File with `HOSTED_PROVIDER_TOKEN=...`, the token abembed's `HOSTED_PROVIDER_TOKEN` secret
        is set to. A path string so the token stays out of the nix store.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.hosted-provider = {
      description = "abembed hosted provider";
      wantedBy = ["multi-user.target"];
      wants = ["network-online.target"];
      after = ["network-online.target"];
      environment =
        {HOSTED_PROVIDER_LISTEN = cfg.listen;}
        // lib.optionalAttrs (cfg.interface != null) {HOSTED_PROVIDER_INTERFACE = cfg.interface;};

      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        EnvironmentFile = cfg.environmentFile;
        Restart = "on-failure";

        DynamicUser = true;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
      };
    };
  };
}
