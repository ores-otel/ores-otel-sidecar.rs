# Shell-free container launcher

The production runtime uses the shared `ores-launcher` from
[ores-otel/ores.otel.log](https://github.com/ores-otel/ores.otel.log).
`docker/ores-launcher.rev` is the sole source revision authority. It pins the
reviewed launcher source from upstream PR #59; no implementation is copied here.
The launcher uses the canonical ores-otel SDK to write a bounded, redacted command
record to stderr, flushes locally, then replaces itself with the application using
Unix exec. It is not an init/reaper and does not decrypt secrets.

The application remains part of ENTRYPOINT, not CMD. Existing argument-only
invocations therefore retain their meaning. To replace the whole executable, use
`docker run --entrypoint /ores-launcher IMAGE /absolute/program [arguments...]`.
The direct application path used by kubelet is unchanged. Probe behavior is not
certified by this change; the related Sonus probe mismatch is tracked in
sonus-auris/sonus-auris-sidecar.rs#7.

Never pass credentials in argv; redaction cannot identify arbitrary positional
secrets. Supply secrets through platform configuration, never build arguments.

The image workflow builds the actual application on native Linux amd64 and arm64
runners. Its host-side smoke script checks metadata, UID/GID 65532, missing-command
exit 64, exec-failure exit 127, canonical stderr records, literal argv logging, and
absent sh/bash under a read-only, network-isolated, capability-free container.
Passing this image contract is not application/DB startup, probe, SIGTERM-draining,
or production deployment certification. Existing application tests remain required.
