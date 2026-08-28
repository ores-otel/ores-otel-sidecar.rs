#![forbid(unsafe_code)]

use ores_otel_sidecar::{SidecarEnv, SidecarIdentity};

fn read(path: &str) -> String {
    let root = env!("CARGO_MANIFEST_DIR");
    std::fs::read_to_string(format!("{root}/{path}")).unwrap_or_else(|err| {
        panic!("read {path}: {err}");
    })
}

#[test]
fn generated_runtimes_match_cli_flags_and_identity() {
    let flags = read(".cli-flags.toml");
    assert!(flags.contains("ORES_OTEL_SIDECAR_BIND"));
    assert!(flags.contains("ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK"));
    assert!(flags.contains("127.0.0.1:9090"));
    assert!(flags.contains("ores-otel-sidecar"));

    for path in [
        "generated/rust/env.rs",
        "generated/typescript/env.ts",
        "generated/dart/env.dart",
        "generated/gleam/env.gleam",
    ] {
        let text = read(path);
        assert!(text.contains("ORES_OTEL_SIDECAR_BIND"), "{path}");
        assert!(
            text.contains("ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK"),
            "{path}"
        );
        assert!(text.contains("127.0.0.1:9090"), "{path}");
        assert!(text.contains("ores-otel-sidecar"), "{path}");
    }

    assert_eq!(SidecarIdentity::ORES_OTEL.bind_env, SidecarEnv::KEYS.bind);
    assert_eq!(
        SidecarIdentity::ORES_OTEL.default_bind,
        SidecarIdentity::DEFAULT_BIND
    );
    assert_eq!(
        SidecarEnv::KEYS.allow_non_loopback,
        "ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK"
    );
}
