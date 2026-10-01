import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

const beamNix = await readFile("proof/consumer-lifecycle-v2/beamscale/bmscl-lifecycle-agent.nix", "utf8");
const beamDocs = await readFile("proof/consumer-lifecycle-v2/beamscale/process-lifecycle.md", "utf8");
const beamReadme = await readFile("proof/consumer-lifecycle-v2/beamscale/README.md", "utf8");
const scintillaNix = await readFile("proof/consumer-lifecycle-v2/scintilla/scintilla-lifecycle-agent.nix", "utf8");
const scintillaReadme = await readFile("proof/consumer-lifecycle-v2/scintilla/README.md", "utf8");

function requireAll(text, values, label) {
  for (const value of values) {
    assert.ok(text.includes(value), `${label} missing required invariant: ${value}`);
  }
}

function forbidAll(text, values, label) {
  for (const value of values) {
    assert.ok(!text.includes(value), `${label} retains forbidden authority: ${value}`);
  }
}

const sharedV2Required = [
  'ORES_PROCESS_LIFECYCLE_CONFIG_VERSION = "v2"',
  'ORES_PROCESS_LIFECYCLE_LEASE_PROVIDER = "cloudflare-do"',
  'ORES_PROCESS_LIFECYCLE_EFFECTS = "observe"',
  'ProtectControlGroups = true',
  'RestrictAddressFamilies = [ "AF_UNIX" ]',
  'CapabilityBoundingSet = [ ]',
  'AmbientCapabilities = [ ]',
];
const sharedV2Forbidden = [
  'ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET',
  'ORES_PROCESS_LIFECYCLE_HOST_CONTROL_SOCKET',
  'ORES_PROCESS_LIFECYCLE_CHECKPOINT_ROOT',
  'ORES_PROCESS_LIFECYCLE_LEASE_BACKEND',
  'ORES_PROCESS_LIFECYCLE_HIBERNATE_ENABLED',
  'ORES_PROCESS_LIFECYCLE_EFFECTS_ENABLED',
  'ORES_PROCESS_LIFECYCLE_LEASE_ENDPOINT',
  'ORES_PROCESS_LIFECYCLE_CREDENTIAL_NAME',
  'ORES_PROCESS_LIFECYCLE_EFFECTS = "freeze_thaw"',
  'LoadCredential',
  'CAP_DAC_OVERRIDE',
  'CAP_SYS_ADMIN',
  'CAP_SYS_PTRACE',
  'ProtectControlGroups = false',
  'RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" ]',
];

requireAll(beamNix, sharedV2Required, "BeamScale Nix");
forbidAll(beamNix, sharedV2Forbidden, "BeamScale Nix");
requireAll(beamNix, [
  'ORES_PROCESS_LIFECYCLE_PRODUCT = "beamscale"',
  'ORES_PROCESS_LIFECYCLE_ENVIRONMENT = cfg.deploymentEnvironment',
  'ORES_PROCESS_LIFECYCLE_REGION = cfg.region',
  'reservedEnvironmentNames = builtins.filter',
  'lib.hasPrefix "ORES_PROCESS_LIFECYCLE_" name',
  'name == "CREDENTIALS_DIRECTORY"',
  'productSocket = "${socketRoot}/product/control.sock"',
  'hostControlSocket = "${socketRoot}/host/control.sock"',
  '"d ${productSocketDirectory} 2770 ${cfg.controlSocketOwner} ${controlGroup} -"',
  '"d ${hostControlSocketDirectory} 0700 root root -"',
  'ReadWritePaths = [ stateRoot ]',
], "BeamScale Nix");
requireAll(beamDocs, [
  'beamscale/runtime-lifecycle/<environment>/<region>/<runtime-id>',
  'Node identity is deliberately excluded',
  'LEASE_PROVIDER=cloudflare-do',
  'EFFECTS=observe',
], "BeamScale lifecycle docs");
forbidAll(beamDocs, [
  'process-lifecycle/beamscale/<cluster>/<workload>',
], "BeamScale lifecycle docs");
requireAll(beamReadme, [
  'Lifecycle config v2 derives both fixed socket paths',
  'callers do not provide separate product- or host-socket path authority',
  'EFFECTS=observe',
  'beamscale/runtime-lifecycle/<environment>/<region>/<runtime-id>',
  '/run/beamscale-lifecycle/product/control.sock',
  '/run/beamscale-lifecycle/host/control.sock',
], "BeamScale README");
forbidAll(beamReadme, [
  'ORES_PROCESS_LIFECYCLE_HOST_CONTROL_SOCKET',
  'ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET',
  'compatibility checkpoint directory',
], "BeamScale README");

requireAll(scintillaNix, sharedV2Required, "Scintilla Nix");
forbidAll(scintillaNix, sharedV2Forbidden, "Scintilla Nix");
requireAll(scintillaNix, [
  'ORES_PROCESS_LIFECYCLE_PRODUCT = "scintilla-run"',
  'ORES_PROCESS_LIFECYCLE_ENVIRONMENT = cfg.environment',
  'ORES_PROCESS_LIFECYCLE_REGION = cfg.region',
  'productSocket = "${socketRoot}/product/control.sock"',
  'hostControlSocket = "${socketRoot}/host/control.sock"',
  '"d ${productSocketDirectory} 2770 ${runtimeCfg.runtimeUser} ${controlGroup} -"',
  '"d ${hostControlSocketDirectory} 0700 root root -"',
  'ReadWritePaths = [ stateRoot ]',
], "Scintilla Nix");
requireAll(scintillaReadme, [
  'Lifecycle config v2 derives both fixed socket paths',
  'callers do not provide separate socket-path authority',
  'EFFECTS=observe',
  '/run/scintilla-lifecycle/product/control.sock',
  '/run/scintilla-lifecycle/host/control.sock',
], "Scintilla README");
forbidAll(scintillaReadme, [
  'ORES_PROCESS_LIFECYCLE_HOST_CONTROL_SOCKET',
  'ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET',
], "Scintilla README");

console.log("consumer lifecycle config v2 exact-file contracts: ok");
