# NixOS module: the txflow bridge and its Cloudflare Tunnel on one relay host,
# for hosts whose config lives in Nix (install.sh does the same elsewhere).
#
#   imports = [ ./txflow-bridge.nix ];
#   services.txflow-bridge = {
#     enable = true;
#     relay = "frankfurt";
#     src = pkgs.fetchFromGitHub { owner = "will-break-it"; repo = "txflow"; rev = "..."; hash = "..."; };
#     cardanoCli = "${cardano-cli}/bin/cardano-cli";   # same Leios release as the node
#     environmentFile = "/run/secrets/txflow-bridge.env";
#   };
#
# environmentFile holds BRIDGE_TOKEN=<32+ characters> and TUNNEL_TOKEN=<the
# tunnel's token>, and stays out of the Nix store (sops-nix, agenix, or a file
# put there by hand). The bridge listens on 127.0.0.1:8164 only, as a
# throwaway user in the node socket's group; cloudflared only dials out, so
# the host opens no port.
{ config, lib, pkgs, ... }:

let
  cfg = config.services.txflow-bridge;
  hardening = {
    DynamicUser = true;
    NoNewPrivileges = true;
    ProtectSystem = "strict";
    ProtectHome = true;
    PrivateTmp = true;
    Restart = "always";
    RestartSec = 5;
  };
in
{
  options.services.txflow-bridge = {
    enable = lib.mkEnableOption "the txflow relay bridge and its tunnel";
    relay = lib.mkOption {
      type = lib.types.str;
      description = "This relay's id in networks/<network>.json.";
    };
    network = lib.mkOption {
      type = lib.types.str;
      default = "musashi";
    };
    src = lib.mkOption {
      type = lib.types.path;
      description = "A txflow checkout (scripts/relay-bridge.mjs and networks/).";
    };
    socket = lib.mkOption {
      type = lib.types.str;
      default = "/run/musashi/node.socket";
    };
    socketGroup = lib.mkOption {
      type = lib.types.str;
      default = "musashi";
      description = "The group that may use the node socket.";
    };
    cardanoCli = lib.mkOption {
      type = lib.types.str;
      description = "Path to cardano-cli from the node's release.";
    };
    traceFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/var/log/musashi/traces/musashi-eu-relay-01_3010/node.json";
      description = ''
        The node's current machine trace (on Musashi, the node.json symlink the
        node repoints at each rotation). With it, /events serves the node's own
        mempool and block events; its directory must be readable by socketGroup.
      '';
    };
    environmentFile = lib.mkOption {
      type = lib.types.str;
      description = "File with BRIDGE_TOKEN= and TUNNEL_TOKEN=, outside the store.";
    };
    tunnel = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Run cloudflared for the tunnel the Worker reaches the bridge through.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.txflow-bridge = {
      description = "txflow relay bridge";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];
      environment = {
        NETWORK = cfg.network;
        RELAY = cfg.relay;
        SOCKET = cfg.socket;
        CARDANO_CLI = cfg.cardanoCli;
        BRIDGE_PORT = "8164";
      } // lib.optionalAttrs (cfg.traceFile != null) {
        TRACE_FILE = cfg.traceFile;
      };
      serviceConfig = hardening // {
        ExecStart = "${pkgs.nodejs_22}/bin/node ${cfg.src}/scripts/relay-bridge.mjs";
        EnvironmentFile = cfg.environmentFile;
        SupplementaryGroups = [ cfg.socketGroup ];
        MemoryMax = "512M";
      };
    };

    systemd.services.txflow-tunnel = lib.mkIf cfg.tunnel {
      description = "txflow bridge tunnel (cloudflared)";
      after = [ "network-online.target" "txflow-bridge.service" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = hardening // {
        ExecStart = "${pkgs.cloudflared}/bin/cloudflared tunnel --no-autoupdate run";
        EnvironmentFile = cfg.environmentFile;
        MemoryMax = "256M";
      };
    };
  };
}
