#![forbid(unsafe_code)]

use flags2env::env_map::{
    resolve_typed_bindings, EnvBindingSpec, EnvDiagnostic, EnvMap, EnvValueKind, ENV_CONTRACT,
};

/// Immutable, typed sidecar startup environment admitted before application initialization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupEnv {
    pub bind: String,
    pub allow_non_loopback: bool,
}

/// Validate the final flags-2-env snapshot before any sidecar runtime is initialized.
///
/// The input must already reflect the canonical precedence order and argv alias normalization.
/// Runtime values are never copied into diagnostics on failure.
pub fn preflight_startup(values: &EnvMap) -> Result<StartupEnv, Vec<EnvDiagnostic>> {
    let specs = [
        EnvBindingSpec::required(
            "bind",
            "ORES_OTEL_SIDECAR_BIND",
            EnvValueKind::String,
        ),
        EnvBindingSpec::required(
            "allow_non_loopback",
            "ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK",
            EnvValueKind::Bool,
        ),
    ];
    let resolved = resolve_typed_bindings(values, &specs)?;

    let Some(bind) = resolved.get("bind").and_then(|value| value.as_str()) else {
        return Err(vec![contract_diagnostic(
            "ORES_OTEL_SIDECAR_BIND",
            "string",
        )]);
    };
    let Some(allow_non_loopback) = resolved
        .get("allow_non_loopback")
        .and_then(|value| value.as_bool())
    else {
        return Err(vec![contract_diagnostic(
            "ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK",
            "bool",
        )]);
    };

    Ok(StartupEnv {
        bind: bind.to_string(),
        allow_non_loopback,
    })
}

fn contract_diagnostic(name: &str, expected: &str) -> EnvDiagnostic {
    EnvDiagnostic {
        code: ENV_CONTRACT,
        name: name.to_string(),
        expected: expected.to_string(),
        secret: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flags2env::env_map::{ENV_MISSING, ENV_PARSE};

    fn valid() -> EnvMap {
        EnvMap::from([
            (
                "ORES_OTEL_SIDECAR_BIND".to_string(),
                "127.0.0.1:9090".to_string(),
            ),
            (
                "ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK".to_string(),
                "false".to_string(),
            ),
        ])
    }

    #[test]
    fn valid_snapshot_returns_typed_config() {
        let config = preflight_startup(&valid()).unwrap();
        assert_eq!(config.bind, "127.0.0.1:9090");
        assert!(!config.allow_non_loopback);
    }

    #[test]
    fn missing_values_fail_before_runtime_init() {
        let errors = preflight_startup(&EnvMap::new()).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().all(|error| error.code == ENV_MISSING));
    }

    #[test]
    fn raw_boolean_aliases_are_not_runtime_boolean_syntax() {
        for value in ["TRUE", "yes", "1", "0", " true", "false "] {
            let mut env = valid();
            env.insert(
                "ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK".to_string(),
                value.to_string(),
            );
            let errors = preflight_startup(&env).unwrap_err();
            assert_eq!(errors.len(), 1, "value {value:?}");
            assert_eq!(errors[0].code, ENV_PARSE);
            assert_eq!(
                errors[0].name,
                "ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK"
            );
        }
    }

    #[test]
    fn empty_bind_is_rejected() {
        let mut env = valid();
        env.insert("ORES_OTEL_SIDECAR_BIND".to_string(), String::new());
        let errors = preflight_startup(&env).unwrap_err();
        assert_eq!(errors[0].code, ENV_PARSE);
    }

    #[test]
    fn diagnostics_never_reflect_runtime_values() {
        let marker = "synthetic-secret-never-reflect";
        let mut env = valid();
        env.insert(
            "ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK".to_string(),
            marker.to_string(),
        );
        let errors = preflight_startup(&env).unwrap_err();
        assert!(!format!("{errors:?}").contains(marker));
    }
}
