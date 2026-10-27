{ config, lib, ... }:

let
  cfg = config.services.bmsclLifecycleAgent;
  controlGroup = "beamscale-lifecycle-control";
  stateRoot = "/var/lib/beamscale/lifecycle";
  managedCgroupRoot = "/sys/fs/cgroup/beamscale-workloads.slice";
  socketRoot = "/run/beamscale-lifecycle";
  productSocket = "${socketRoot}/product/control.sock";
  productSocketDirectory = builtins.dirOf productSocket;
  hostControlSocket = "${socketRoot}/host/control.sock";
  hostControlSocketDirectory = builtins.dirOf hostControlSocket;
  productUnit = "${cfg.productService}.service";
  reservedEnvironmentNames = builtins.filter
    (name:
      lib.hasPrefix "ORES_PROCESS_LIFECYCLE_" name
      || name == "CREDENTIALS_DIRECTORY")
    (builtins.attrNames cfg.environment);
in
{
  options.services.bmsclLifecycleAgent = {
    enable = lib.mkEnableOption "BeamScale external host lifecycle observer/preflight agent";

    package = lib.mkOption {
      type = lib.types.package;
      description = "Pinned package containing ores-process-lifecycle-agent with lifecycle config v2 support.";
    };

    binary = lib.mkOption {
      type = lib.types.str;
      default = "ores-process-lifecycle-agent";
    };

    cluster = lib.mkOption {
      type = lib.types.str;
      description = "Stable BeamScale cluster/cell identity recorded by trusted host control.";
    };

    deploymentEnvironment = lib.mkOption {
      type = lib.types.str;
      description = "Stable deployment environment component of the canonical BeamScale lifecycle lease namespace.";
    };

    region = lib.mkOption {
      type = lib.types.str;
      description = "Stable region component of the canonical BeamScale lifecycle lease namespace.";
    };

    node = lib.mkOption {
      type = lib.types.str;
      default = config.networking.hostName;
      description = "Stable host/controller identity; never part of the logical lifecycle lease key.";
    };

    productService = lib.mkOption {
      type = lib.types.str;
      description = "Existing NixOS systemd service name, without .service, that runs bmscl-supervisor. The lifecycle module injects the fixed product socket into this exact service and orders itself after it.";
    };

    controlSocketOwner = lib.mkOption {
      type = lib.types.str;
      description = "Exact OS user that runs bmscl-supervisor and owns the private cooperative lifecycle socket directory. This must be supplied by the host deployment; do not guess a tenant or runtime user.";
    };

    reconcileSeconds = lib.mkOption {
      type = lib.types.ints.positive;
      default = 15;
      description = "Observe-only reconciliation cadence while the trusted mutation runtime remains disabled.";
    };

    environment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = {};
      description = "Additional non-secret lifecycle-agent environment. Reserved ORES lifecycle authority fields and CREDENTIALS_DIRECTORY are rejected by this observe-only module.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          builtins.match "^[A-Za-z0-9][A-Za-z0-9._:-]{0,95}$" cfg.cluster != null;
        message = "BeamScale lifecycle cluster must be a safe shared-agent identity segment";
      }
      {
        assertion =
          builtins.match "^[A-Za-z0-9][A-Za-z0-9._:-]{0,95}$" cfg.deploymentEnvironment != null;
        message = "BeamScale lifecycle deploymentEnvironment must be a safe lease identity segment";
      }
      {
        assertion =
          builtins.match "^[A-Za-z0-9][A-Za-z0-9._:-]{0,95}$" cfg.region != null;
        message = "BeamScale lifecycle region must be a safe lease identity segment";
      }
      {
        assertion =
          builtins.match "^[A-Za-z0-9][A-Za-z0-9._:-]{0,95}$" cfg.node != null;
        message = "BeamScale lifecycle node must be a safe shared-agent identity segment";
      }
      {
        assertion =
          builtins.match "^[A-Za-z_][A-Za-z0-9_-]{0,63}$" cfg.controlSocketOwner != null;
        message = "BeamScale lifecycle controlSocketOwner must be an explicit safe OS user name";
      }
      {
        assertion =
          builtins.match "^[A-Za-z0-9][A-Za-z0-9_.@-]{0,95}$" cfg.productService != null
          && cfg.productService != "bmscl-lifecycle-agent";
        message = "BeamScale lifecycle productService must name the existing non-lifecycle bmscl-supervisor systemd service without .service";
      }
      {
        assertion = reservedEnvironmentNames == [];
        message = "BeamScale lifecycle extra environment must not inject reserved ORES lifecycle authority or systemd credential fields";
      }
    ];

    users.groups.${controlGroup} = {};

    systemd.slices."beamscale-workloads" = {
      description = "BeamScale suspendable workload processes";
      sliceConfig = {
        CPUAccounting = true;
        MemoryAccounting = true;
        TasksAccounting = true;
      };
    };

    systemd.tmpfiles.rules = [
      "d ${stateRoot} 0700 root root -"
      # Product and trusted host-control sockets must never share a writable
      # parent. Directory write permission is sufficient to unlink/squat a Unix
      # socket entry even when the socket inode itself is root-owned.
      "d ${socketRoot} 0755 root root -"
      "d ${productSocketDirectory} 2770 ${cfg.controlSocketOwner} ${controlGroup} -"
      "d ${hostControlSocketDirectory} 0700 root root -"
    ];

    # Enabling lifecycle observation must also enable the cooperative product
    # bridge on the exact host service. This is deliberately product-local and
    # non-secret: distributed lifecycle authority remains outside the supervisor.
    systemd.services.${cfg.productService}.environment = {
      BMSCL_LIFECYCLE_SOCKET = productSocket;
    };
    systemd.services.${cfg.productService}.serviceConfig.ReadWritePaths =
      lib.mkAfter [ productSocketDirectory ];
    systemd.services.${cfg.productService}.serviceConfig.InaccessiblePaths =
      lib.mkAfter [ hostControlSocketDirectory ];

    systemd.services.bmscl-lifecycle-agent = {
      description = "BeamScale external host lifecycle observer/preflight agent";
      wantedBy = [ "multi-user.target" ];
      requires = [
        "beamscale-workloads.slice"
        productUnit
      ];
      after = [
        "beamscale-workloads.slice"
        productUnit
      ];

      # Lifecycle config v2 derives the fixed cooperative/trusted socket paths
      # from the BeamScale product adapter. Observe mode deliberately carries no
      # lease endpoint, bearer credential, checkpoint authority, or IP network.
      environment = cfg.environment // {
        ORES_PROCESS_LIFECYCLE_CONFIG_VERSION = "v2";
        ORES_PROCESS_LIFECYCLE_PRODUCT = "beamscale";
        ORES_PROCESS_LIFECYCLE_CLUSTER = cfg.cluster;
        ORES_PROCESS_LIFECYCLE_ENVIRONMENT = cfg.deploymentEnvironment;
        ORES_PROCESS_LIFECYCLE_REGION = cfg.region;
        ORES_PROCESS_LIFECYCLE_NODE = cfg.node;
        ORES_PROCESS_LIFECYCLE_STATE_ROOT = stateRoot;
        ORES_PROCESS_LIFECYCLE_CGROUP_ROOT = managedCgroupRoot;
        ORES_PROCESS_LIFECYCLE_RECONCILE_SECONDS = toString cfg.reconcileSeconds;
        ORES_PROCESS_LIFECYCLE_LEASE_PROVIDER = "cloudflare-do";
        ORES_PROCESS_LIFECYCLE_EFFECTS = "observe";
      };

      serviceConfig = {
        User = "root";
        Group = "root";
        SupplementaryGroups = [ controlGroup ];
        ExecStartPre = [
          "${cfg.package}/bin/${cfg.binary} preflight"
          "${cfg.package}/bin/${cfg.binary} probe-product"
        ];
        ExecStart = "${cfg.package}/bin/${cfg.binary}";
        Restart = "always";
        RestartSec = "2s";
        UMask = "0077";

        # Observe/preflight-only means no cgroup write, lease network, DAC-bypass,
        # checkpoint, ptrace, mount, namespace, or credential authority is needed.
        NoNewPrivileges = true;
        CapabilityBoundingSet = [ ];
        AmbientCapabilities = [ ];
        PrivateDevices = true;
        PrivateTmp = true;
        ProtectClock = true;
        ProtectControlGroups = true;
        ProtectHome = true;
        ProtectHostname = true;
        ProtectKernelLogs = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectProc = "invisible";
        ProtectSystem = "strict";
        ProcSubset = "pid";
        RestrictAddressFamilies = [ "AF_UNIX" ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        RemoveIPC = true;

        ReadOnlyPaths = [
          "/sys/fs/cgroup"
          hostControlSocketDirectory
        ];
        ReadWritePaths = [ stateRoot ];
      };
    };
  };
}
