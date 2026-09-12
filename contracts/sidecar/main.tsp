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
  @maxValue(180)
  poll_seconds: int32;
  @minLength(1)
  @maxLength(64)
  @pattern("^[A-Za-z0-9._-]+$")
  `namespace`: string;
  @minLength(1)
  @maxLength(64)
  @pattern("^[A-Za-z0-9._-]+$")
  cache: string;
}

@id("RuntimeValueDefinition")
model RuntimeValueDefinition {
  @minLength(1)
  @maxLength(128)
  @pattern("^[A-Z_][A-Z0-9_]*$")
  key: string;
  @maxLength(65536)
  value: string;
}

@id("SidecarDefinition")
model SidecarDefinition {
  @minLength(1)
  @maxLength(96)
  @pattern("^[A-Za-z0-9._-]+$")
  id: string;
  @minLength(1)
  @maxLength(256)
  path?: string;
  @minLength(1)
  @maxLength(256)
  startup_config?: string;
  runtime_updates: RuntimeUpdatePolicy;
  @maxItems(128)
  runtime_mutable?: string[];
  @maxItems(128)
  values?: RuntimeValueDefinition[];
}

@id("OresSidecarFile")
model OresSidecarFile {
  schema: "ores.sidecar/config/v1";
  @minItems(1)
  @maxItems(64)
  sidecars: SidecarDefinition[];
}
