#![forbid(unsafe_code)]

use ores_otel_sidecar::process_lifecycle_runtime::{
    resolve_process, run, DEFAULT_LIFECYCLE_CLI_CONFIG_PATH,
};

fn main() {
    let invocation = match resolve_process(DEFAULT_LIFECYCLE_CLI_CONFIG_PATH) {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!(
                "{{\"event\":\"process_lifecycle_invalid_configuration\",\"error\":\"{error}\"}}"
            );
            std::process::exit(2);
        }
    };

    if let Err(error) = run(invocation) {
        eprintln!(
            "{{\"event\":\"process_lifecycle_agent_failed_closed\",\"error\":\"{error}\"}}"
        );
        std::process::exit(3);
    }
}
