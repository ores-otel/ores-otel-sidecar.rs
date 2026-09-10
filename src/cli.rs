#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use flags2env::BundledFlags2Env;

/// Repository-root flags-2-env schema used by the sidecar binaries.
pub const DEFAULT_CONFIG_PATH: &str = ".cli-flags.toml";

/// The only process modes accepted by the sidecar executable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarCommand {
    Serve,
    Preflight,
    ProbeHealthz,
    ProbeReadyz,
}

/// A validated command plus the immutable environment snapshot after defaults,
/// dotenv, process environment, dotenv overrides, and argv precedence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliResolution {
    pub command: SidecarCommand,
    values: BTreeMap<String, String>,
}

impl CliResolution {
    pub fn value(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned()
    }

    /// Final immutable environment snapshot. Defaults are materialized before
    /// higher-precedence sources so startup validation sees exactly one value.
    #[must_use]
    pub fn values(&self) -> &BTreeMap<String, String> {
        &self.values
    }
}

/// Payload-free CLI failure categories safe to map to closed diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliError {
    InvalidConfiguration,
    InvalidArguments,
    ParserUnavailable,
}

impl Display for CliError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidConfiguration => "sidecar CLI configuration is invalid",
            Self::InvalidArguments => "sidecar CLI arguments are invalid",
            Self::ParserUnavailable => "sidecar CLI parser is unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CliError {}

/// Resolve the current process through the canonical bundled flags-2-env parser.
pub fn resolve_process(config_path: &str) -> Result<CliResolution, CliError> {
    resolve(
        &std::env::args().collect::<Vec<_>>(),
        std::env::vars(),
        config_path,
    )
}

/// Resolve explicit argv and environment inputs. This is pure apart from reading
/// the explicitly selected flags-2-env schema and any schema-declared env files.
pub fn resolve<I>(
    argv: &[String],
    process_env: I,
    config_path: &str,
) -> Result<CliResolution, CliError>
where
    I: IntoIterator<Item = (String, String)>,
{
    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(config_path))
        .map_err(|_| CliError::InvalidConfiguration)?;
    let parsed = parser
        .parse_structured(argv, Some(config_path))
        .map_err(|_| CliError::ParserUnavailable)?;

    if !parsed.errors.is_empty() || !parsed.unknown_options.is_empty() || !parsed.extras.is_empty()
    {
        return Err(CliError::InvalidArguments);
    }

    let command = match parsed.command.as_str() {
        "" => SidecarCommand::Serve,
        "preflight" => SidecarCommand::Preflight,
        "probe" => SidecarCommand::ProbeHealthz,
        "probe-readyz" => SidecarCommand::ProbeReadyz,
        _ => return Err(CliError::InvalidArguments),
    };

    // The fully-resolved flags map gives us schema defaults as the lowest
    // layer. Reapplying the structured source channels then produces the
    // canonical precedence:
    // default < dotenv < process environment < dotenv overrides < argv.
    let mut values = BTreeMap::new();
    values.extend(parsed.flags);
    values.extend(parsed.dotenv);
    values.extend(process_env);
    values.extend(parsed.dotenv_overrides);
    values.extend(parsed.provided_flags);

    Ok(CliResolution { command, values })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_path() -> String {
        format!("{}/.cli-flags.toml", env!("CARGO_MANIFEST_DIR"))
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn no_command_materializes_defaults_before_server_mode() {
        let resolved = resolve(
            &argv(&["ores-otel-sidecar"]),
            std::iter::empty(),
            &config_path(),
        )
        .expect("resolve server mode");

        assert_eq!(resolved.command, SidecarCommand::Serve);
        assert_eq!(
            resolved.value("ORES_OTEL_SIDECAR_BIND").as_deref(),
            Some("127.0.0.1:9090")
        );
        assert_eq!(
            resolved
                .value("ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK")
                .as_deref(),
            Some("false")
        );
    }

    #[test]
    fn process_environment_overrides_materialized_defaults() {
        let resolved = resolve(
            &argv(&["ores-otel-sidecar"]),
            [("ORES_OTEL_SIDECAR_BIND".into(), "127.0.0.1:19090".into())],
            &config_path(),
        )
        .expect("resolve server mode");

        assert_eq!(
            resolved.value("ORES_OTEL_SIDECAR_BIND").as_deref(),
            Some("127.0.0.1:19090")
        );
    }

    #[test]
    fn preflight_command_is_first_class() {
        let resolved = resolve(
            &argv(&["ores-otel-sidecar", "preflight"]),
            std::iter::empty(),
            &config_path(),
        )
        .expect("resolve preflight mode");
        assert_eq!(resolved.command, SidecarCommand::Preflight);
    }

    #[test]
    fn probe_aliases_resolve_to_canonical_modes() {
        for (name, expected) in [
            ("probe", SidecarCommand::ProbeHealthz),
            ("probe-healthz", SidecarCommand::ProbeHealthz),
            ("probe-readyz", SidecarCommand::ProbeReadyz),
        ] {
            let resolved = resolve(
                &argv(&["ores-otel-sidecar", name]),
                std::iter::empty(),
                &config_path(),
            )
            .expect("resolve probe mode");
            assert_eq!(resolved.command, expected);
        }
    }

    #[test]
    fn argv_flags_override_process_environment_after_a_command() {
        let resolved = resolve(
            &argv(&["ores-otel-sidecar", "probe", "--bind=127.0.0.1:19191"]),
            [("ORES_OTEL_SIDECAR_BIND".into(), "127.0.0.1:19090".into())],
            &config_path(),
        )
        .expect("resolve argv override");

        assert_eq!(resolved.command, SidecarCommand::ProbeHealthz);
        assert_eq!(
            resolved.value("ORES_OTEL_SIDECAR_BIND").as_deref(),
            Some("127.0.0.1:19191")
        );
    }

    #[test]
    fn argv_boolean_alias_is_normalized_but_raw_environment_is_not() {
        let from_argv = resolve(
            &argv(&["ores-otel-sidecar", "--allow-non-loopback=true"]),
            std::iter::empty(),
            &config_path(),
        )
        .expect("resolve argv bool");
        assert_eq!(
            from_argv
                .value("ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK")
                .as_deref(),
            Some("true")
        );

        let from_env = resolve(
            &argv(&["ores-otel-sidecar"]),
            [("ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK".into(), "yes".into())],
            &config_path(),
        )
        .expect("resolution does not itself reinterpret raw env");
        assert_eq!(
            from_env
                .value("ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK")
                .as_deref(),
            Some("yes")
        );
    }

    #[test]
    fn unknown_options_and_unexpected_operands_fail_closed() {
        for args in [
            argv(&["ores-otel-sidecar", "--not-declared"]),
            argv(&["ores-otel-sidecar", "probe", "unexpected"]),
            argv(&["ores-otel-sidecar", "Authorization=Bearer-synthetic-secret"]),
        ] {
            assert_eq!(
                resolve(&args, std::iter::empty(), &config_path()),
                Err(CliError::InvalidArguments)
            );
        }
    }
}
