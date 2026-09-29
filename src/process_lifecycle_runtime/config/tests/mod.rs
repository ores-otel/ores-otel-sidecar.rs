use super::*;

fn values(product: &str) -> BTreeMap<String, String> {
    let (product_socket, host_control_socket) = if product == "beamscale" {
        (
            "/run/beamscale-lifecycle/product/control.sock",
            "/run/beamscale-lifecycle/host/control.sock",
        )
    } else {
        (
            "/run/scintilla-lifecycle/product/control.sock",
            "/run/scintilla-lifecycle/host/control.sock",
        )
    };
    return BTreeMap::from([
        ("ORES_PROCESS_LIFECYCLE_PRODUCT".to_owned(), product.to_owned()),
        ("ORES_PROCESS_LIFECYCLE_CLUSTER".to_owned(), "prod-us-east".to_owned()),
        ("ORES_PROCESS_LIFECYCLE_NODE".to_owned(), "host-7".to_owned()),
        (
            "ORES_PROCESS_LIFECYCLE_STATE_ROOT".to_owned(),
            "/var/lib/ores-lifecycle/state".to_owned(),
        ),
        (
            "ORES_PROCESS_LIFECYCLE_CHECKPOINT_ROOT".to_owned(),
            "/var/lib/ores-lifecycle/checkpoints".to_owned(),
        ),
        (
            "ORES_PROCESS_LIFECYCLE_CGROUP_ROOT".to_owned(),
            "/sys/fs/cgroup/ores-workloads.slice".to_owned(),
        ),
        (
            "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET".to_owned(),
            product_socket.to_owned(),
        ),
        (
            "ORES_PROCESS_LIFECYCLE_HOST_CONTROL_SOCKET".to_owned(),
            host_control_socket.to_owned(),
        ),
        (
            "ORES_PROCESS_LIFECYCLE_RECONCILE_SECONDS".to_owned(),
            "15".to_owned(),
        ),
        (
            "ORES_PROCESS_LIFECYCLE_LEASE_BACKEND".to_owned(),
            "cloudflare-do".to_owned(),
        ),
        (
            "ORES_PROCESS_LIFECYCLE_HIBERNATE_ENABLED".to_owned(),
            "false".to_owned(),
        ),
        (
            "ORES_PROCESS_LIFECYCLE_EFFECTS_ENABLED".to_owned(),
            "false".to_owned(),
        ),
    ]);
}

fn v2_values(product: &str) -> BTreeMap<String, String> {
    let mut values = values(product);
    values.insert(
        "ORES_PROCESS_LIFECYCLE_CONFIG_VERSION".to_owned(),
        "v2".to_owned(),
    );
    values.insert(
        "ORES_PROCESS_LIFECYCLE_ENVIRONMENT".to_owned(),
        "prod".to_owned(),
    );
    values.insert(
        "ORES_PROCESS_LIFECYCLE_REGION".to_owned(),
        "us-east".to_owned(),
    );
    values.insert(
        "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET".to_owned(),
        String::new(),
    );
    values.insert(
        "ORES_PROCESS_LIFECYCLE_HOST_CONTROL_SOCKET".to_owned(),
        String::new(),
    );
    values.insert(
        "ORES_PROCESS_LIFECYCLE_LEASE_PROVIDER".to_owned(),
        "cloudflare-do".to_owned(),
    );
    values.insert(
        "ORES_PROCESS_LIFECYCLE_EFFECTS".to_owned(),
        "observe".to_owned(),
    );
    return values;
}

#[test]
fn v1_remains_the_default_contract() {
    let config = LifecycleAgentConfig::from_values(&values("beamscale"));
    assert!(matches!(
        config,
        Ok(LifecycleAgentConfig {
            config_version: LifecycleConfigVersion::V1,
            effects: LifecycleEffectsMode::Observe,
            ..
        })
    ));
}

#[test]
fn product_and_host_sockets_are_exact_product_specific_and_separate() {
    assert!(LifecycleAgentConfig::from_values(&values("beamscale")).is_ok());
    let invalid_product = values("beamscale")
        .into_iter()
        .map(|(key, value)| {
            if key == "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET" {
                return (
                    key,
                    "/run/scintilla-lifecycle/product/control.sock".to_owned(),
                );
            }
            return (key, value);
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        LifecycleAgentConfig::from_values(&invalid_product),
        Err(LifecycleRuntimeError::InvalidProductSocket)
    );

    let invalid_host = values("beamscale")
        .into_iter()
        .map(|(key, value)| {
            if key == "ORES_PROCESS_LIFECYCLE_HOST_CONTROL_SOCKET" {
                return (
                    key,
                    "/run/scintilla-lifecycle/host/control.sock".to_owned(),
                );
            }
            return (key, value);
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        LifecycleAgentConfig::from_values(&invalid_host),
        Err(LifecycleRuntimeError::InvalidHostControlSocket)
    );
}

#[test]
fn v2_derives_sockets_and_canonical_product_prefix() {
    let config = LifecycleAgentConfig::from_values(&v2_values("scintilla-run"));
    assert!(matches!(
        config,
        Ok(LifecycleAgentConfig {
            config_version: LifecycleConfigVersion::V2,
            product_socket,
            host_control_socket,
            environment: Some(ref environment),
            region: Some(ref region),
            ..
        }) if product_socket == PathBuf::from("/run/scintilla-lifecycle/product/control.sock")
            && host_control_socket == PathBuf::from("/run/scintilla-lifecycle/host/control.sock")
            && environment == "prod"
            && region == "us-east"
    ));
    assert_eq!(
        LifecycleAgentConfig::from_values(&v2_values("beamscale"))
            .map(|config| config.lease_key_prefix()),
        Ok("beamscale/runtime-lifecycle")
    );
}

#[test]
fn v2_observe_does_not_require_lease_secret_authority() {
    let config = LifecycleAgentConfig::from_values(&v2_values("beamscale"));
    assert!(matches!(
        config,
        Ok(LifecycleAgentConfig {
            effects: LifecycleEffectsMode::Observe,
            effects_enabled: false,
            lease_endpoint: None,
            credential_name: None,
            ..
        })
    ));
}

#[test]
fn v2_freeze_thaw_requires_valid_endpoint_and_credential_name() {
    let mut active = v2_values("beamscale");
    active.insert(
        "ORES_PROCESS_LIFECYCLE_EFFECTS".to_owned(),
        "freeze_thaw".to_owned(),
    );
    assert_eq!(
        LifecycleAgentConfig::from_values(&active),
        Err(LifecycleRuntimeError::MissingLeaseAuthority)
    );

    active.insert(
        "ORES_PROCESS_LIFECYCLE_LEASE_ENDPOINT".to_owned(),
        "https://locks.example.test/".to_owned(),
    );
    active.insert(
        "ORES_PROCESS_LIFECYCLE_CREDENTIAL_NAME".to_owned(),
        "ores-locks-api-token".to_owned(),
    );
    assert!(matches!(
        LifecycleAgentConfig::from_values(&active),
        Ok(LifecycleAgentConfig {
            effects: LifecycleEffectsMode::FreezeThaw,
            effects_enabled: true,
            hibernate_enabled: false,
            lease_endpoint: Some(ref endpoint),
            credential_name: Some(ref credential),
            ..
        }) if endpoint == "https://locks.example.test"
            && credential == "ores-locks-api-token"
    ));

    active.insert(
        "ORES_PROCESS_LIFECYCLE_LEASE_ENDPOINT".to_owned(),
        "http://locks.example.test".to_owned(),
    );
    assert_eq!(
        LifecycleAgentConfig::from_values(&active),
        Err(LifecycleRuntimeError::InvalidLeaseEndpoint)
    );
}

#[test]
fn v2_rejects_legacy_mutation_switches_and_socket_authority() {
    let mut invalid = v2_values("beamscale");
    invalid.insert(
        "ORES_PROCESS_LIFECYCLE_EFFECTS_ENABLED".to_owned(),
        "true".to_owned(),
    );
    assert_eq!(
        LifecycleAgentConfig::from_values(&invalid),
        Err(LifecycleRuntimeError::InvalidArguments)
    );

    let mut invalid_socket = v2_values("beamscale");
    invalid_socket.insert(
        "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET".to_owned(),
        "/run/beamscale-lifecycle/product/control.sock".to_owned(),
    );
    assert_eq!(
        LifecycleAgentConfig::from_values(&invalid_socket),
        Err(LifecycleRuntimeError::InvalidArguments)
    );
}

#[test]
fn shared_or_nested_socket_parent_is_rejected() {
    assert_eq!(
        validate_socket_trust_split(
            Path::new("/run/example/control.sock"),
            Path::new("/run/example/host-control.sock")
        ),
        Err(LifecycleRuntimeError::InvalidPath)
    );
    assert_eq!(
        validate_socket_trust_split(
            Path::new("/run/example/product/control.sock"),
            Path::new("/run/example/product/host/control.sock")
        ),
        Err(LifecycleRuntimeError::InvalidPath)
    );
}

#[test]
fn lifecycle_identity_rejects_path_separators() {
    let invalid = values("scintilla-run")
        .into_iter()
        .map(|(key, value)| {
            if key == "ORES_PROCESS_LIFECYCLE_CLUSTER" {
                return (key, "prod/escape".to_owned());
            }
            return (key, value);
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        LifecycleAgentConfig::from_values(&invalid),
        Err(LifecycleRuntimeError::InvalidIdentity)
    );
}

#[test]
fn cgroup_root_cannot_expand_to_host_root() {
    let invalid = values("beamscale")
        .into_iter()
        .map(|(key, value)| {
            if key == "ORES_PROCESS_LIFECYCLE_CGROUP_ROOT" {
                return (key, "/sys/fs/cgroup".to_owned());
            }
            return (key, value);
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        LifecycleAgentConfig::from_values(&invalid),
        Err(LifecycleRuntimeError::InvalidPath)
    );
}

#[test]
fn effects_flag_is_explicitly_preserved_for_v1() {
    let enabled = values("beamscale")
        .into_iter()
        .map(|(key, value)| {
            if key == "ORES_PROCESS_LIFECYCLE_EFFECTS_ENABLED" {
                return (key, "true".to_owned());
            }
            return (key, value);
        })
        .collect::<BTreeMap<_, _>>();
    assert!(matches!(
        LifecycleAgentConfig::from_values(&enabled),
        Ok(LifecycleAgentConfig {
            config_version: LifecycleConfigVersion::V1,
            effects_enabled: true,
            ..
        })
    ));
}
