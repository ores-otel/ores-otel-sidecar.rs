use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::adapters::process_lifecycle_freeze::FreezeTransitionEffects;
use crate::adapters::process_lifecycle_identity::{
    ExpectedLinuxProcessIdentity, LinuxProcessIdentityAttestor,
};
use crate::process_lifecycle_agent::LifecycleEffects;
use crate::process_lifecycle_record::LifecycleCheckpoint;

use super::*;

const PID: u32 = 4242;
const START_TICKS: u64 = 987_654;

struct Fixture {
    root: PathBuf,
    proc_root: PathBuf,
    cgroup_mount: PathBuf,
    managed_cgroup: PathBuf,
    other_cgroup: PathBuf,
}

impl Fixture {
    fn new(name: &str, frozen: bool) -> io::Result<Self> {
        let root = std::env::temp_dir().join(format!(
            "ores-attested-freeze-{name}-{}",
            std::process::id()
        ));
        match fs::remove_dir_all(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error);
            }
        }

        let proc_root = root.join("proc");
        let cgroup_mount = root.join("sys/fs/cgroup");
        let process_dir = proc_root.join(PID.to_string());
        let managed_cgroup = cgroup_mount.join("beamscale-workloads.slice/workload-7.scope");
        let other_cgroup = cgroup_mount.join("beamscale-workloads.slice/other.scope");
        fs::create_dir_all(&process_dir)?;
        fs::create_dir_all(&managed_cgroup)?;
        fs::create_dir_all(&other_cgroup)?;
        write_process_stat(&process_dir, START_TICKS)?;
        fs::write(
            process_dir.join("cgroup"),
            b"0::/beamscale-workloads.slice/workload-7.scope\n",
        )?;
        write_cgroup(&managed_cgroup, frozen, PID)?;
        write_cgroup(&other_cgroup, false, 9999)?;

        return Ok(Self {
            root,
            proc_root,
            cgroup_mount,
            managed_cgroup,
            other_cgroup,
        });
    }

    fn identity(&self) -> ExpectedLinuxProcessIdentity {
        return ExpectedLinuxProcessIdentity {
            pid: PID,
            process_start_ticks: START_TICKS,
            managed_cgroup: self.managed_cgroup.clone(),
        };
    }

    fn attestor(&self) -> LinuxProcessIdentityAttestor {
        return LinuxProcessIdentityAttestor::new(&self.proc_root, &self.cgroup_mount);
    }

    fn effects(&self) -> AttestedFreezeOnlyCgroupEffects {
        return AttestedFreezeOnlyCgroupEffects::with_attestor_and_timing(
            self.identity(),
            self.attestor(),
            Duration::from_millis(10),
            Duration::from_millis(1),
        );
    }

    fn process_dir(&self) -> PathBuf {
        return self.proc_root.join(PID.to_string());
    }

    fn cleanup(self) -> io::Result<()> {
        return fs::remove_dir_all(self.root);
    }
}

fn write_process_stat(process_dir: &Path, start_ticks: u64) -> io::Result<()> {
    return fs::write(
        process_dir.join("stat"),
        format!(
            "{PID} (worker) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start_ticks} 0\n"
        ),
    );
}

fn write_cgroup(path: &Path, frozen: bool, pid: u32) -> io::Result<()> {
    let frozen_flag = if frozen { 1 } else { 0 };
    fs::write(
        path.join("cgroup.events"),
        format!("populated 1\nfrozen {frozen_flag}\n"),
    )?;
    fs::write(path.join("cgroup.freeze"), b"0\n")?;
    return fs::write(path.join("cgroup.procs"), format!("{pid}\n"));
}

#[test]
fn freeze_targets_only_the_attested_cgroup() -> io::Result<()> {
    let fixture = Fixture::new("exact-target", true)?;
    let mut effects = fixture.effects();

    assert_eq!(LifecycleEffects::freeze(&mut effects), Ok(()));
    assert_eq!(fs::read(fixture.managed_cgroup.join("cgroup.freeze"))?, b"1\n");
    assert_eq!(fs::read(fixture.other_cgroup.join("cgroup.freeze"))?, b"0\n");

    fixture.cleanup()?;
    return Ok(());
}

#[test]
fn stale_process_start_identity_blocks_effect_before_write() -> io::Result<()> {
    let fixture = Fixture::new("stale-start", true)?;
    write_process_stat(&fixture.process_dir(), START_TICKS + 1)?;
    let mut effects = fixture.effects();

    assert!(LifecycleEffects::freeze(&mut effects).is_err());
    assert_eq!(fs::read(fixture.managed_cgroup.join("cgroup.freeze"))?, b"0\n");

    fixture.cleanup()?;
    return Ok(());
}

#[test]
fn cgroup_migration_blocks_effect_before_write() -> io::Result<()> {
    let fixture = Fixture::new("migrated", true)?;
    fs::write(
        fixture.process_dir().join("cgroup"),
        b"0::/beamscale-workloads.slice/other.scope\n",
    )?;
    let mut effects = fixture.effects();

    assert!(LifecycleEffects::freeze(&mut effects).is_err());
    assert_eq!(fs::read(fixture.managed_cgroup.join("cgroup.freeze"))?, b"0\n");

    fixture.cleanup()?;
    return Ok(());
}

#[test]
fn recovery_status_is_identity_attested() -> io::Result<()> {
    let fixture = Fixture::new("status", true)?;
    let mut effects = fixture.effects();
    assert_eq!(
        FreezeTransitionEffects::freeze_status(&mut effects),
        Ok(FreezeTransitionStatus {
            populated: true,
            frozen: true,
        })
    );

    write_process_stat(&fixture.process_dir(), START_TICKS + 1)?;
    assert!(FreezeTransitionEffects::freeze_status(&mut effects).is_err());

    fixture.cleanup()?;
    return Ok(());
}

#[test]
fn hibernate_authority_remains_unavailable() -> io::Result<()> {
    let fixture = Fixture::new("hibernate", false)?;
    let mut effects = fixture.effects();
    let checkpoint = LifecycleCheckpoint {
        artifact_ref: "test://checkpoint".to_owned(),
        digest: "sha256:test".to_owned(),
        format: "test".to_owned(),
    };

    assert!(LifecycleEffects::checkpoint_and_terminate(&mut effects).is_err());
    assert!(LifecycleEffects::restore(&mut effects, &checkpoint).is_err());

    fixture.cleanup()?;
    return Ok(());
}
