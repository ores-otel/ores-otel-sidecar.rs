#![forbid(unsafe_code)]

use flags2env::env_map::{
    resolve_typed_bindings, EnvBindingSpec, EnvDiagnostic, EnvMap, EnvValueKind, ENV_CONTRACT,
};

use crate::identity::{ALLOW_NON_LOOPBACK, BIND};

/// Immutable, typed sidecar startup environment admitted before application initialization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupEnv {
    pub bind: String,
    pub allow_non_loopback: bool,
}

/// Validate the canonical ORES-OTel sidecar startup keys.
///
/// Product sidecars with generated product-specific keys should call
/// [`preflight_startup_with_keys`] instead.
pub fn preflight_startup(values: &EnvMap) -> Result<StartupEnv, Vec<EnvDiagnostic>> {
    preflight_startup_with_keys(values, BIND, ALLOW_NON_LOOPBACK)
}

/// Validate a product sidecar's final flags-2-env snapshot before any runtime is initialized.
///
/// The input must already reflect the canonical precedence order and argv alias normalization.
/// Runtime values are never copied into diagnostics on failure.
pub fn preflight_startup_with_keys(
    values: &EnvMap,
    bind_env: &str,
    allow_non_loopback_env: &str,
) -> Result<StartupEnv, Vec<EnvDiagnostic>> {
    let specs = [
        EnvBindingSpec::required("bind", bind_env, EnvValueKind::String),
        EnvBindingSpec::required(
            "allow_non_loopback",
            allow_non_loopback_env,
            EnvValueKind::Bool,
        ),
    ];
    let resolved = resolve_typed_bindings(values, &specs)?;

    let Some(bind) = resolved.get("bind").and_then(|value| value.as_str()) else {
        return Err(vec![contract_diagnostic(bind_env, "string")]);
    };
    let Some(allow_non_loopback) = resolved
        .get("allow_non_loopback")
        .and_then(|value| value.as_bool())
    else {
        return Err(vec![contract_diagnostic(allow_non_loopback_env, "bool")]);
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
            (BIND.to_string(), "127.0.0.1:9090".to_string()),
            (ALLOW_NON_LOOPBACK.to_string(), "false".to_string()),
        ])
    }

    #[test]
    fn valid_snapshot_returns_typed_config() {
        let config = preflight_startup(&valid()).unwrap();
        assert_eq!(config.bind, "127.0.0.1:9090");
        assert!(!config.allow_non_loopback);
    }

    #[test]
    fn product_specific_keys_use_same_preflight() {
        let env = EnvMap::from([
            ("PRODUCT_BIND".to_string(), "127.0.0.1:19191".to_string()),
            ("PRODUCT_ALLOW".to_string(), "true".to_string()),
        ]);
        let config = preflight_startup_with_keys(&env, "PRODUCT_BIND", "PRODUCT_ALLOW").unwrap();
        assert_eq!(config.bind, "127.0.0.1:19191");
        assert!(config.allow_non_loopback);
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
            env.insert(ALLOW_NON_LOOPBACK.to_string(), value.to_string());
            let errors = preflight_startup(&env).unwrap_err();
            assert_eq!(errors.len(), 1, "value {value:?}");
            assert_eq!(errors[0].code, ENV_PARSE);
            assert_eq!(errors[0].name, ALLOW_NON_LOOPBACK);
        }
    }

    #[test]
    fn empty_bind_is_rejected() {
        let mut env = valid();
        env.insert(BIND.to_string(), String::new());
        let errors = preflight_startup(&env).unwrap_err();
        assert_eq!(errors[0].code, ENV_PARSE);
    }

    #[test]
    fn diagnostics_never_reflect_runtime_values() {
        let marker = "synthetic-secret-never-reflect";
        let mut env = valid();
        env.insert(ALLOW_NON_LOOPBACK.to_string(), marker.to_string());
        let errors = preflight_startup(&env).unwrap_err();
        assert!(!format!("{errors:?}").contains(marker));
    }
}
