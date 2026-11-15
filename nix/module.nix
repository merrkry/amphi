{
  config,
  lib,
  options,
  pkgs,
  ...
}:

let
  cfg = config.services.amphi;
  toml = pkgs.formats.toml { };

  credentialsOption =
    unit:
    lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      example = {
        token = "/run/secrets/amphi/token";
      };
      description = ''
        Files passed to the service with `LoadCredential`, by name. Refer to
        them in the settings as `/run/credentials/${unit}.service/<name>`, so
        the service user does not need access to the original files.
      '';
    };

  loadCredentials = lib.mapAttrsToList (name: path: "${name}:${path}");

  # The web UI serves the packaged frontend unless the settings name other assets.
  serverSettings = lib.recursiveUpdate cfg.server.settings (
    lib.optionalAttrs (cfg.server.settings ? web && !(cfg.server.settings.web ? assets)) {
      web.assets = "${cfg.webPackage}";
    }
  );
in
{
  options.services.amphi = {
    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.amphi;
      defaultText = lib.literalExpression "pkgs.amphi";
      description = "The Amphi package.";
    };

    webPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.amphi-web;
      defaultText = lib.literalExpression "pkgs.amphi-web";
      description = "The built Amphi web UI.";
    };

    server = {
      enable = lib.mkEnableOption "the Amphi build coordinator";

      settings = lib.mkOption {
        inherit (toml) type;
        default = { };
        description = "Server configuration. See `src/server/config.rs` in the Amphi repository.";
      };

      credentials = credentialsOption "amphi-server";

      path = lib.mkOption {
        type = lib.types.listOf lib.types.package;
        default = [ ];
        description = "Extra packages on the server's PATH.";
      };

      alloyForwardTo = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "prometheus.remote_write.default.receiver" ];
        description = ''
          Alloy receivers for the server's metrics. When set, Alloy scrapes
          `metrics-listen` through a component in `/etc/alloy/amphi.alloy`.
        '';
      };
    };

    worker = {
      enable = lib.mkEnableOption "an Amphi build worker";

      settings = lib.mkOption {
        inherit (toml) type;
        default = { };
        description = "Worker configuration. See `src/worker.rs` in the Amphi repository.";
      };

      credentials = credentialsOption "amphi-worker";

      niks3Package = lib.mkOption {
        type = lib.types.package;
        description = "Package providing the `niks3` command that uploads outputs.";
      };
    };
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.server.enable {
      services.amphi.server.settings = {
        database-url = lib.mkDefault "postgres:///amphi?host=/run/postgresql&user=amphi";
        state-dir = lib.mkDefault "/var/lib/amphi";
      };

      # nix-eval-jobs evaluates flakes, and `nix copy` stages derivations.
      nix.settings.experimental-features = [
        "nix-command"
        "flakes"
      ];

      services.postgresql = {
        enable = true;
        ensureDatabases = [ "amphi" ];
        ensureUsers = [
          {
            name = "amphi";
            ensureDBOwnership = true;
          }
        ];
      };

      users = {
        users.amphi = {
          isSystemUser = true;
          group = "amphi";
          home = "/var/lib/amphi";
        };
        groups.amphi = { };
      };

      systemd.services.amphi-server = {
        description = "Amphi build coordinator";
        wantedBy = [ "multi-user.target" ];
        after = [
          "network-online.target"
          "postgresql-setup.service"
        ];
        wants = [ "network-online.target" ];
        requires = [ "postgresql-setup.service" ];
        path = [
          config.nix.package
          pkgs.nix-eval-jobs
          # `nix copy` to ssh-ng stores.
          pkgs.openssh
          pkgs.git
        ]
        ++ cfg.server.path;
        environment.HOME = "/var/lib/amphi";
        serviceConfig = {
          ExecStart = "${lib.getExe' cfg.package "amphi-server"} --config ${toml.generate "amphi-server.toml" serverSettings}";
          User = "amphi";
          Group = "amphi";
          StateDirectory = "amphi";
          LoadCredential = loadCredentials cfg.server.credentials;
          Restart = "on-failure";
          RestartSec = 10;
        };
      };
    })

    (lib.mkIf (cfg.server.enable && cfg.server.alloyForwardTo != [ ]) {
      assertions = [
        {
          assertion = cfg.server.settings ? metrics-listen;
          message = "services.amphi.server.alloyForwardTo requires the metrics-listen setting.";
        }
      ];

      environment.etc."alloy/amphi.alloy".text = ''
        prometheus.scrape "amphi" {
          targets = [{
            __address__ = "${cfg.server.settings.metrics-listen or ""}",
            instance    = "${config.networking.hostName}",
            job         = "amphi",
          }]
          scrape_interval = "30s"
          forward_to      = [${lib.concatStringsSep ", " cfg.server.alloyForwardTo}]
        }
      '';
    })

    # Publishes builds made on the server's host outside Amphi, which
    # evaluation counts as available. Needs niks3's auto-upload module.
    (lib.optionalAttrs (options ? services.niks3-auto-upload) (
      lib.mkIf cfg.server.enable {
        services.niks3-auto-upload = {
          enable = lib.mkDefault true;
          serverUrl = lib.mkIf (cfg.server.settings ? niks3) (
            lib.mkDefault cfg.server.settings.niks3.server-url
          );
        };
      }
    ))

    (lib.mkIf cfg.worker.enable {
      users = {
        users.amphi-worker = {
          isSystemUser = true;
          group = "amphi-worker";
          home = "/var/lib/amphi-worker";
        };
        groups.amphi-worker = { };
      };

      nix.settings = {
        # Builds run through `nix build`.
        experimental-features = [ "nix-command" ];
        # Only trusted users may disable remote builders for their builds.
        trusted-users = [ "amphi-worker" ];
      };

      systemd.services.amphi-worker = {
        description = "Amphi build worker";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        path = [
          config.nix.package
          cfg.worker.niks3Package
        ];
        environment.HOME = "/var/lib/amphi-worker";
        serviceConfig = {
          ExecStart = "${lib.getExe' cfg.package "amphi-worker"} --config ${toml.generate "amphi-worker.toml" cfg.worker.settings}";
          User = "amphi-worker";
          Group = "amphi-worker";
          StateDirectory = "amphi-worker";
          LoadCredential = loadCredentials cfg.worker.credentials;
          # Running builds belong to the Nix daemon, which cancels them when
          # the worker disconnects. The server requeues them.
          Restart = "always";
          RestartSec = 10;
        };
      };
    })
  ];
}
