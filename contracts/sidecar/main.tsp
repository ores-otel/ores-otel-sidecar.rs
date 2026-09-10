import "@typespec/json-schema";
using TypeSpec.JsonSchema;

namespace Ores.Sidecar;

@id("RuntimeUpdateProvider")
enum RuntimeUpdateProvider {
  disabled: "disabled",
  oresRedisLruCache: "ores-redis-lru-cache",
}

@id("RuntimeUpdateMode")
enum RuntimeUpdateMode {
  disabled: "disabled",
  receiveOnly: "receive-only",
}

@id("RuntimeUpdatePolicy")
model RuntimeUpdatePolicy {
  provider: RuntimeUpdateProvider;
  mode: RuntimeUpdateMode;
  @minValue(60)
  @maxValue(3600)
  poll_seconds: int32;
  namespace: string;
}

@id("RuntimeValueDefinition")
model RuntimeValueDefinition {
  key: string;
  value: string;
}

@id("SidecarDefinition")
model SidecarDefinition {
  id: string;
  path?: string;
  startup_config?: string;
  runtime_updates: RuntimeUpdatePolicy;
  runtime_mutable?: string[];
  values?: RuntimeValueDefinition[];
}

@id("OresSidecarFile")
model OresSidecarFile {
  schema: "ores.sidecar/config/v1";
  sidecars: SidecarDefinition[];
}
