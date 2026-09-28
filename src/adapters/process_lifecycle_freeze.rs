//! Freeze-only Linux lifecycle effects.
//!
//! This adapter intentionally implements only the first process-mutation tranche:
//! cgroup-v2 freeze/thaw with kernel-confirmed completion. Checkpoint/restore is
//! rejected unconditionally so composing this adapter cannot accidentally grant
//! CRIU hibernation authority.

#![forbid(unsafe_code)]

use std::time::Duration;

use crate::process_lifecycle_agent::LifecycleEffects;
use crate::process_lifecycle_record::LifecycleCheckpoint;

use super::linux_process_lifecycle::CgroupV2Controller;

const DEFAULT_EFFECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const HIBERNATE_DISABLED: &str = "hibernate effects are disabled for freeze-only lifecycle adapter";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreezeOnlyCgroupEffects {
    controller: CgroupV2Controller,
    effect_timeout: Duration,
    poll_interval: Duration,
}

impl FreezeOnlyCgroupEffects {
    #[must_use]
    pub fn new(controller: CgroupV2Controller) -> Self {
        return Self {
            controller,
            effect_timeout: DEFAULT_EFFECT_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
        };
    }

    #[must_use]
    pub fn with_timing(
        controller: CgroupV2Controller,
        effect_timeout: Duration,
        poll_interval: Duration,
    ) -> Self {
        return Self {
            controller,
            effect_timeout,
            poll_interval,
        };
    }
}

impl LifecycleEffects for FreezeOnlyCgroupEffects {
    fn freeze(&mut self) -> Result<(), String> {
        self.controller
            .request_freeze()
            .map_err(|error| error.to_string())?;
        return self
            .controller
            .wait_until_frozen(self.effect_timeout, self.poll_interval)
            .map_err(|error| error.to_string());
    }

    fn thaw(&mut self) -> Result<(), String> {
        self.controller
            .request_thaw()
            .map_err(|error| error.to_string())?;
        return self
            .controller
            .wait_until_thawed(self.effect_timeout, self.poll_interval)
            .map_err(|error| error.to_string());
    }

    fn checkpoint_and_terminate(&mut self) -> Result<LifecycleCheckpoint, String> {
        return Err(HIBERNATE_DISABLED.to_owned());
    }

    fn restore(&mut self, _checkpoint: &LifecycleCheckpoint) -> Result<(), String> {
        return Err(HIBERNATE_DISABLED.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;

    fn fixture_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ores-lifecycle-freeze-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create fixture cgroup");
        return root;
    }

    fn write_events(root: &Path, frozen: bool) {
        let frozen_flag = if frozen { 1 } else { 0 };
        fs::write(
            root.join("cgroup.events"),
            format!("populated 1\nfrozen {frozen_flag}\n"),
        )
        .expect("write cgroup.events");
        fs::write(root.join("cgroup.freeze"), b"0\n").expect("write cgroup.freeze");
    }

    #[test]
    fn freeze_waits_for_kernel_confirmed_frozen_state() {
        let root = fixture_dir("freeze");
        write_events(&root, true);
        let controller = CgroupV2Controller::new(&root);
        let mut effects = FreezeOnlyCgroupEffects::with_timing(
            controller,
            Duration::from_millis(10),
            Duration::from_millis(1),
        );

        assert_eq!(effects.freeze(), Ok(()));
        assert_eq!(fs::read(root.join("cgroup.freeze")).unwrap(), b"1\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn thaw_waits_for_kernel_confirmed_thawed_state() {
        let root = fixture_dir("thaw");
        write_events(&root, false);
        let controller = CgroupV2Controller::new(&root);
        let mut effects = FreezeOnlyCgroupEffects::with_timing(
            controller,
            Duration::from_millis(10),
            Duration::from_millis(1),
        );

        assert_eq!(effects.thaw(), Ok(()));
        assert_eq!(fs::read(root.join("cgroup.freeze")).unwrap(), b"0\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hibernate_operations_are_unconditionally_rejected() {
        let root = fixture_dir("hibernate-disabled");
        write_events(&root, false);
        let controller = CgroupV2Controller::new(&root);
        let mut effects = FreezeOnlyCgroupEffects::new(controller);

        assert_eq!(
            effects.checkpoint_and_terminate(),
            Err(HIBERNATE_DISABLED.to_owned())
        );

        let checkpoint = LifecycleCheckpoint {
            artifact_ref: "test://checkpoint".to_owned(),
            digest: "sha256:test".to_owned(),
            format: "test".to_owned(),
        };
        assert_eq!(
            effects.restore(&checkpoint),
            Err(HIBERNATE_DISABLED.to_owned())
        );
        fs::remove_dir_all(root).unwrap();
    }
}
