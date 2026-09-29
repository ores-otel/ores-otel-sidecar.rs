use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::*;

const PID: u32 = 4242;
const START_TICKS: u64 = 987_654;

struct Fixture {
    root: PathBuf,
    proc_root: PathBuf,
    cgroup_mount: PathBuf,
    managed_cgroup: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> io::Result<Self> {
        let root = std::env::temp_dir().join(format!(
            "ores-lifecycle-identity-{name}-{}",
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
        fs::create_dir_all(&process_dir)?;
        fs::create_dir_all(&managed_cgroup)?;
        write_process_stat(&process_dir, START_TICKS, "worker name ) safe")?;
        fs::write(
            process_dir.join("cgroup"),
            b"0::/beamscale-workloads.slice/workload-7.scope\n",
        )?;
        fs::write(managed_cgroup.join("cgroup.procs"), format!("{PID}\n"))?;
        write_secure_status(&process_dir)?;
        write_namespace_links(&process_dir)?;
        return Ok(Self {
            root,
            proc_root,
            cgroup_mount,
            managed_cgroup,
        });
    }

    fn attestor(&self) -> LinuxProcessIdentityAttestor {
        return LinuxProcessIdentityAttestor::new(&self.proc_root, &self.cgroup_mount);
    }

    fn identity(&self) -> ExpectedLinuxProcessIdentity {
        return ExpectedLinuxProcessIdentity {
            pid: PID,
            process_start_ticks: START_TICKS,
            managed_cgroup: self.managed_cgroup.clone(),
        };
    }

    fn process_dir(&self) -> PathBuf {
        return self.proc_root.join(PID.to_string());
    }

    fn cleanup(self) -> io::Result<()> {
        return fs::remove_dir_all(self.root);
    }
}

fn write_process_stat(process_dir: &Path, start_ticks: u64, comm: &str) -> io::Result<()> {
    // Linux /proc/<pid>/stat fields 3..22. The parser intentionally anchors on
    // the final ')' so spaces and ')' inside comm do not shift starttime.
    return fs::write(
        process_dir.join("stat"),
        format!(
            "{PID} ({comm}) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start_ticks} 0\n"
        ),
    );
}

fn write_secure_status(process_dir: &Path) -> io::Result<()> {
    fs::write(
        process_dir.join("status"),
        concat!(
            "Name:\tworker\n",
            "NoNewPrivs:\t1\n",
            "CapInh:\t0000000000000000\n",
            "CapPrm:\t0000000000000000\n",
            "CapEff:\t0000000000000000\n",
            "CapBnd:\t0000000000000000\n",
            "CapAmb:\t0000000000000000\n",
        ),
    )
}

#[cfg(unix)]
fn write_namespace_links(process_dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::symlink;

    let ns = process_dir.join("ns");
    fs::create_dir_all(&ns)?;
    for (kind, inode) in [("pid", 201_u64), ("user", 202), ("mnt", 203), ("net", 204)] {
        symlink(format!("{kind}:[{inode}]"), ns.join(kind))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn write_namespace_links(_process_dir: &Path) -> io::Result<()> {
    Ok(())
}

fn isolation(fixture: &Fixture) -> ExpectedLinuxIsolationEnvelope {
    ExpectedLinuxIsolationEnvelope {
        process: fixture.identity(),
        pid_namespace_inode: 201,
        user_namespace_inode: 202,
        mount_namespace_inode: 203,
        network_namespace_inode: 204,
    }
}

#[test]
fn exact_process_start_and_cgroup_membership_attest() -> io::Result<()> {
    let fixture = Fixture::new("valid")?;
    assert_eq!(
        fixture.attestor().attest(&fixture.identity()),
        Ok(fixture.identity())
    );
    fixture.cleanup()?;
    return Ok(());
}

#[test]
fn reused_pid_is_rejected_by_start_ticks() -> io::Result<()> {
    let fixture = Fixture::new("pid-reuse")?;
    write_process_stat(&fixture.process_dir(), START_TICKS + 1, "replacement")?;
    assert_eq!(
        fixture.attestor().attest(&fixture.identity()),
        Err(LinuxProcessIdentityError::ProcessStartMismatch)
    );
    fixture.cleanup()?;
    return Ok(());
}

#[test]
fn process_reported_in_another_cgroup_is_rejected() -> io::Result<()> {
    let fixture = Fixture::new("foreign-cgroup")?;
    fs::write(
        fixture.process_dir().join("cgroup"),
        b"0::/beamscale-workloads.slice/replacement.scope\n",
    )?;
    assert_eq!(
        fixture.attestor().attest(&fixture.identity()),
        Err(LinuxProcessIdentityError::ProcessCgroupMismatch)
    );
    fixture.cleanup()?;
    return Ok(());
}

#[test]
fn cgroup_procs_must_contain_the_exact_pid() -> io::Result<()> {
    let fixture = Fixture::new("procs-membership")?;
    fs::write(fixture.managed_cgroup.join("cgroup.procs"), b"9999\n")?;
    assert_eq!(
        fixture.attestor().attest(&fixture.identity()),
        Err(LinuxProcessIdentityError::ProcessNotInManagedCgroup)
    );
    fixture.cleanup()?;
    return Ok(());
}

#[test]
fn duplicate_v2_membership_records_fail_closed() -> io::Result<()> {
    let fixture = Fixture::new("duplicate-v2")?;
    fs::write(
        fixture.process_dir().join("cgroup"),
        b"0::/beamscale-workloads.slice/workload-7.scope\n0::/other.scope\n",
    )?;
    assert_eq!(
        fixture.attestor().attest(&fixture.identity()),
        Err(LinuxProcessIdentityError::InvalidProcCgroup)
    );
    fixture.cleanup()?;
    return Ok(());
}

#[test]
fn managed_cgroup_must_be_beneath_the_configured_mount() -> io::Result<()> {
    let fixture = Fixture::new("escape")?;
    let invalid = ExpectedLinuxProcessIdentity {
        managed_cgroup: fixture.root.join("outside.scope"),
        ..fixture.identity()
    };
    assert_eq!(
        fixture.attestor().attest(&invalid),
        Err(LinuxProcessIdentityError::InvalidExpectedIdentity)
    );
    fixture.cleanup()?;
    return Ok(());
}

#[cfg(unix)]
#[test]
fn symlinked_managed_cgroup_is_rejected() -> io::Result<()> {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new("cgroup-symlink")?;
    let real = fixture.cgroup_mount.join("real.scope");
    let link = fixture.cgroup_mount.join("link.scope");
    fs::create_dir_all(&real)?;
    fs::write(real.join("cgroup.procs"), format!("{PID}\n"))?;
    symlink(&real, &link)?;
    let invalid = ExpectedLinuxProcessIdentity {
        managed_cgroup: link,
        ..fixture.identity()
    };
    assert_eq!(
        fixture.attestor().attest(&invalid),
        Err(LinuxProcessIdentityError::InvalidManagedCgroupDirectory)
    );
    fixture.cleanup()?;
    return Ok(());
}

#[cfg(unix)]
#[test]
fn exact_namespace_and_privilege_envelope_attests() -> io::Result<()> {
    let fixture = Fixture::new("isolation-valid")?;
    let expected = isolation(&fixture);
    assert_eq!(
        fixture.attestor().attest_isolation(&expected),
        Ok(AttestedLinuxIsolationEnvelope {
            process: fixture.identity(),
            pid_namespace_inode: 201,
            user_namespace_inode: 202,
            mount_namespace_inode: 203,
            network_namespace_inode: 204,
            no_new_privs: true,
            capability_sets_empty: true,
        })
    );
    fixture.cleanup()?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn namespace_identity_mismatch_fails_closed() -> io::Result<()> {
    let fixture = Fixture::new("namespace-mismatch")?;
    let mut expected = isolation(&fixture);
    expected.network_namespace_inode = 999;
    assert_eq!(
        fixture.attestor().attest_isolation(&expected),
        Err(LinuxProcessIdentityError::NamespaceMismatch)
    );
    fixture.cleanup()?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn no_new_privs_must_be_enabled() -> io::Result<()> {
    let fixture = Fixture::new("nnp-disabled")?;
    let status = concat!(
        "Name:\tworker\n",
        "NoNewPrivs:\t0\n",
        "CapInh:\t0000000000000000\n",
        "CapPrm:\t0000000000000000\n",
        "CapEff:\t0000000000000000\n",
        "CapBnd:\t0000000000000000\n",
        "CapAmb:\t0000000000000000\n",
    );
    fs::write(fixture.process_dir().join("status"), status)?;
    assert_eq!(
        fixture.attestor().attest_isolation(&isolation(&fixture)),
        Err(LinuxProcessIdentityError::NoNewPrivsDisabled)
    );
    fixture.cleanup()?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn every_linux_capability_set_must_be_empty() -> io::Result<()> {
    for (name, field) in [
        ("cap-inh", "CapInh"),
        ("cap-prm", "CapPrm"),
        ("cap-eff", "CapEff"),
        ("cap-bnd", "CapBnd"),
        ("cap-amb", "CapAmb"),
    ] {
        let fixture = Fixture::new(name)?;
        let status = [
            "Name:\tworker".to_owned(),
            "NoNewPrivs:\t1".to_owned(),
            format!("CapInh:\t{}", if field == "CapInh" { "0000000000000001" } else { "0000000000000000" }),
            format!("CapPrm:\t{}", if field == "CapPrm" { "0000000000000001" } else { "0000000000000000" }),
            format!("CapEff:\t{}", if field == "CapEff" { "0000000000000001" } else { "0000000000000000" }),
            format!("CapBnd:\t{}", if field == "CapBnd" { "0000000000000001" } else { "0000000000000000" }),
            format!("CapAmb:\t{}", if field == "CapAmb" { "0000000000000001" } else { "0000000000000000" }),
        ]
        .join("\n");
        fs::write(fixture.process_dir().join("status"), format!("{status}\n"))?;
        assert_eq!(
            fixture.attestor().attest_isolation(&isolation(&fixture)),
            Err(LinuxProcessIdentityError::CapabilitiesNotEmpty),
            "non-empty {field} must fail"
        );
        fixture.cleanup()?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn malformed_or_duplicate_security_status_fails_closed() -> io::Result<()> {
    let fixture = Fixture::new("bad-status")?;
    fs::write(
        fixture.process_dir().join("status"),
        concat!(
            "NoNewPrivs:\t1\n",
            "NoNewPrivs:\t1\n",
            "CapInh:\t0000000000000000\n",
            "CapPrm:\t0000000000000000\n",
            "CapEff:\t0000000000000000\n",
            "CapBnd:\t0000000000000000\n",
            "CapAmb:\t0000000000000000\n",
        ),
    )?;
    assert_eq!(
        fixture.attestor().attest_isolation(&isolation(&fixture)),
        Err(LinuxProcessIdentityError::InvalidProcStatus)
    );
    fixture.cleanup()?;
    Ok(())
}

#[test]
fn stat_parser_handles_spaces_and_parentheses_in_comm() {
    let input = format!(
        "{PID} (strange worker ) name) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {START_TICKS} 0\n"
    );
    assert_eq!(
        parse_proc_stat_start_ticks(&input, PID),
        Ok(START_TICKS)
    );
}
