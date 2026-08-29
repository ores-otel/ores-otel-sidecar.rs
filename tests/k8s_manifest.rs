#![forbid(unsafe_code)]

fn read(path: &str) -> String {
    let root = env!("CARGO_MANIFEST_DIR");
    std::fs::read_to_string(format!("{root}/{path}")).unwrap()
}

fn assert_unintrusive(manifest: &str) {
    assert!(
        manifest.contains("127.0.0.1:9090"),
        "sidecar must bind loopback so it does not join the app Service"
    );
    assert!(
        !manifest.contains("0.0.0.0"),
        "unspecified bind would expose probes on the pod IP"
    );
    assert!(
        manifest.contains("exec:"),
        "kubelet httpGet uses the pod IP and cannot see loopback"
    );
    assert!(
        manifest.contains("probe"),
        "exec must call the same binary's probe command (no curl in distroless)"
    );
    assert!(
        !manifest.contains("httpGet:"),
        "httpGet against the pod IP is the wrong probe for a loopback listener"
    );
    assert!(
        !manifest.contains("readinessProbe:"),
        "sidecar readiness would remove the app from Service endpoints"
    );
    assert!(
        !manifest.contains("hostPort:"),
        "hostPort would steal a node port"
    );
    assert!(
        manifest.contains("stdin: false"),
        "sidecar must not take stdin from the pod"
    );
    assert!(manifest.contains("drop:"), "capabilities must be dropped");
    let lower = manifest.to_ascii_lowercase();
    for forbidden in [
        "aws_access_key",
        "aws_secret",
        "google_application_credentials",
        "azure_client_secret",
        "client_secret",
        "bearer_token",
    ] {
        assert!(
            !lower.contains(forbidden),
            "sidecar manifests must use platform collection, not static credential {forbidden}"
        );
    }
}

#[test]
fn container_snippet_is_unintrusive_on_k8s_cluster() {
    assert_unintrusive(&read("k8s/container.yaml"));
    let snippet = read("k8s/container.yaml");
    assert!(!snippet.contains("containerPort:"));
}

#[test]
fn example_pod_keeps_app_http_port_separate() {
    let pod = read("k8s/pod.example.yaml");
    assert_unintrusive(&pod);
    assert!(pod.contains("containerPort: 8080"));
    assert!(pod.contains("shareProcessNamespace: false"));
    assert!(!pod.contains("containerPort: 9090"));
}
