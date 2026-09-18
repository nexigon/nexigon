{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    optionalAttrs
    optional
    types
    ;
  cfg = config.services.nexigon-agent;
  toml = pkgs.formats.toml { };
  fingerprintScript = pkgs.writeShellScript "nexigon-device-fingerprint" ''
    exec ${pkgs.coreutils}/bin/cat /etc/machine-id
  '';
  configFile = toml.generate "nexigon-agent.toml" cfg.settings;
in

{
  options.services.nexigon-agent = {
    enable = mkEnableOption "the Nexigon on-device agent";

    package = lib.mkPackageOption pkgs "nexigon-agent" { };

    dataPath = mkOption {
      type = types.externalPath;
      default = "/var/lib/nexigon/agent";
      description = "Persistent directory for provisioned credentials and agent state.";
    };

    settings = mkOption {
      inherit (toml) type;
      default = { };
      description = ''
        Nexigon Agent configuration. Do not put deployment tokens or other secrets
        here because this configuration is copied from the world-readable Nix store.
      '';
    };

    provisioning = {
      enable = mkEnableOption "pairing-key provisioning";

      listenAddress = mkOption {
        type = types.str;
        default = "0.0.0.0";
        description = "Address on which the pairing-key provisioning endpoint listens.";
      };

      port = mkOption {
        type = types.port;
        default = 6947;
        description = "TCP port for the pairing-key provisioning endpoint.";
      };

      endpoints = mkOption {
        type = types.listOf types.str;
        default = [
          "https://eu.nexigon.cloud"
          "https://us.nexigon.cloud"
        ];
        description = "Nexigon Hub endpoints tried for unqualified pairing keys.";
      };

      deviceName = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional device name assigned while redeeming a pairing key.";
      };

      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = "Whether to open the provisioning TCP port in the firewall.";
      };
    };
  };

  config = mkIf cfg.enable {
    warnings = optional (cfg.settings ? token) ''
      services.nexigon-agent.settings.token is stored in the Nix store. Use
      pairing-key provisioning or provision credentials in ${cfg.dataPath} instead.
    '';

    services.nexigon-agent.settings = {
      fingerprint-script = lib.mkDefault "${fingerprintScript}";
      data-path = lib.mkDefault cfg.dataPath;
      provisioning = mkIf cfg.provisioning.enable (
        {
          enabled = true;
          bind = "${cfg.provisioning.listenAddress}:${toString cfg.provisioning.port}";
          inherit (cfg.provisioning) endpoints;
        }
        // optionalAttrs (cfg.provisioning.deviceName != null) {
          device-name = cfg.provisioning.deviceName;
        }
      );
    };

    networking.firewall.allowedTCPPorts = mkIf (
      cfg.provisioning.enable && cfg.provisioning.openFirewall
    ) [ cfg.provisioning.port ];

    systemd.tmpfiles.rules = [ "d ${cfg.dataPath} 0700 root root -" ];

    systemd.services.nexigon-agent = {
      description = "Nexigon Agent";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      path = [
        pkgs.bash
        pkgs.coreutils
        pkgs.systemd
      ];
      serviceConfig = {
        Type = "exec";
        RuntimeDirectory = "nexigon";
        RuntimeDirectoryMode = "0700";
        ExecStartPre = "${pkgs.coreutils}/bin/install -m 0600 ${configFile} /run/nexigon/agent.toml";
        ExecStart = "${cfg.package}/bin/nexigon-agent --config /run/nexigon/agent.toml run";
        Restart = "always";
        RestartSec = "60s";
        LimitNOFILE = 8192;
      };
    };
  };
}
