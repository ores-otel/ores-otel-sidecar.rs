//! Portable process/filesystem APM resource collection for product sidecars.
//!
//! This module intentionally exposes logical filesystem labels (`fs0`, `fs1`, …)
//! instead of mount paths. Product sidecars may export the measurements without
//! leaking host/container filesystem layout. Runtime-specific heap/GC/event-loop
//! metrics remain the responsibility of language adapters.

use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use sysinfo::{
    get_current_pid, DiskRefreshKind, Disks, Pid, ProcessRefreshKind, ProcessesToUpdate, System,
    IS_SUPPORTED_SYSTEM,
};

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ProcessResourceSnapshot {
    pub cpu_time_seconds: f64,
    pub cpu_utilization_ratio: Option<f64>,
    pub cpu_sample_warmed: bool,
    pub memory_usage_bytes: u64,
    pub memory_virtual_bytes: u64,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub uptime_seconds: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct FilesystemResourceSnapshot {
    /// Stable only within one scrape result. Never a raw path or device name.
    pub target: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub utilization_ratio: f64,
    pub read_only: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ResourceSnapshot {
    pub process: ProcessResourceSnapshot,
    pub filesystems: Vec<FilesystemResourceSnapshot>,
}

#[derive(Debug)]
pub struct ResourceCollector {
    system: System,
    disks: Disks,
    pid: Pid,
    warmed: bool,
    logical_cpus: usize,
}

impl ResourceCollector {
    pub fn new() -> Option<Self> {
        if !IS_SUPPORTED_SYSTEM {
            return None;
        }
        let pid = get_current_pid().ok()?;
        let refresh = process_refresh_kind();
        let mut system = System::new();
        let pids = [pid];
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            false,
            refresh,
        );
        let disks = Disks::new_with_refreshed_list_specifics(
            DiskRefreshKind::nothing().with_storage(),
        );
        let logical_cpus = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .max(1);
        Some(Self {
            system,
            disks,
            pid,
            warmed: false,
            logical_cpus,
        })
    }

    pub fn collect(&mut self) -> Option<ResourceSnapshot> {
        let pids = [self.pid];
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            false,
            process_refresh_kind(),
        );
        self.disks.refresh_specifics(
            true,
            DiskRefreshKind::nothing().with_storage(),
        );

        let process = self.system.process(self.pid)?;
        let disk = process.disk_usage();
        let warmed = self.warmed;
        self.warmed = true;
        let cpu_ratio = warmed.then(|| {
            ((f64::from(process.cpu_usage()) / 100.0) / self.logical_cpus as f64)
                .clamp(0.0, 1.0)
        });

        let mut disks = self.disks.list().iter().collect::<Vec<_>>();
        disks.sort_by(|left, right| left.mount_point().cmp(right.mount_point()));
        let filesystems = disks
            .into_iter()
            .enumerate()
            .map(|(index, disk)| {
                let total = disk.total_space();
                let available = disk.available_space().min(total);
                let used = total.saturating_sub(available);
                let utilization_ratio = if total == 0 {
                    0.0
                } else {
                    (used as f64 / total as f64).clamp(0.0, 1.0)
                };
                FilesystemResourceSnapshot {
                    target: format!("fs{index}"),
                    total_bytes: total,
                    used_bytes: used,
                    available_bytes: available,
                    utilization_ratio,
                    read_only: disk.is_read_only(),
                }
            })
            .collect();

        Some(ResourceSnapshot {
            process: ProcessResourceSnapshot {
                cpu_time_seconds: process.accumulated_cpu_time() as f64 / 1_000.0,
                cpu_utilization_ratio: cpu_ratio,
                cpu_sample_warmed: warmed,
                memory_usage_bytes: process.memory(),
                memory_virtual_bytes: process.virtual_memory(),
                disk_read_bytes: disk.total_read_bytes,
                disk_write_bytes: disk.total_written_bytes,
                uptime_seconds: process.run_time(),
            },
            filesystems,
        })
    }
}

fn process_refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_disk_usage()
        .without_tasks()
}

static COLLECTOR: OnceLock<Mutex<Option<ResourceCollector>>> = OnceLock::new();

/// Render a bounded Prometheus/OpenMetrics-compatible text projection.
///
/// The collector is lazy: processes that never scrape `/metrics` pay no sysinfo
/// initialization cost. A poisoned collector fails closed with `supported=0`.
pub fn prometheus_text(service: &str) -> String {
    let collector = COLLECTOR.get_or_init(|| Mutex::new(ResourceCollector::new()));
    let mut guard = match collector.lock() {
        Ok(guard) => guard,
        Err(_) => return unsupported_metrics(),
    };
    let Some(collector) = guard.as_mut() else {
        return unsupported_metrics();
    };
    let Some(snapshot) = collector.collect() else {
        return unsupported_metrics();
    };
    render_prometheus(service, &snapshot)
}

fn unsupported_metrics() -> String {
    concat!(
        "# HELP ores_otel_resource_collector_supported Whether portable process/filesystem collection is supported.\n",
        "# TYPE ores_otel_resource_collector_supported gauge\n",
        "ores_otel_resource_collector_supported 0\n",
    )
    .to_owned()
}

pub fn render_prometheus(service: &str, snapshot: &ResourceSnapshot) -> String {
    let service = escape_label(service);
    let process = &snapshot.process;
    let mut out = String::new();
    out.push_str("# HELP ores_otel_resource_collector_supported Whether portable process/filesystem collection is supported.\n");
    out.push_str("# TYPE ores_otel_resource_collector_supported gauge\n");
    out.push_str("ores_otel_resource_collector_supported 1\n");
    out.push_str("# HELP ores_otel_process_cpu_time_seconds Total accumulated process CPU time.\n");
    out.push_str("# TYPE ores_otel_process_cpu_time_seconds counter\n");
    out.push_str(&format!("ores_otel_process_cpu_time_seconds{{service=\"{service}\"}} {}\n", process.cpu_time_seconds));
    out.push_str("# HELP ores_otel_process_cpu_sample_warmed Whether process CPU utilization has a prior sample.\n");
    out.push_str("# TYPE ores_otel_process_cpu_sample_warmed gauge\n");
    out.push_str(&format!("ores_otel_process_cpu_sample_warmed{{service=\"{service}\"}} {}\n", u8::from(process.cpu_sample_warmed)));
    if let Some(cpu) = process.cpu_utilization_ratio {
        out.push_str("# HELP ores_otel_process_cpu_utilization_ratio Process CPU utilization normalized to logical CPU capacity.\n");
        out.push_str("# TYPE ores_otel_process_cpu_utilization_ratio gauge\n");
        out.push_str(&format!("ores_otel_process_cpu_utilization_ratio{{service=\"{service}\"}} {cpu}\n"));
    }
    push_u64_metric(
        &mut out,
        "ores_otel_process_memory_usage_bytes",
        "Resident process memory usage in bytes.",
        "gauge",
        &service,
        process.memory_usage_bytes,
    );
    push_u64_metric(
        &mut out,
        "ores_otel_process_memory_virtual_bytes",
        "Virtual process memory usage in bytes.",
        "gauge",
        &service,
        process.memory_virtual_bytes,
    );
    push_u64_metric(
        &mut out,
        "ores_otel_process_disk_read_bytes_total",
        "Cumulative bytes read by the process. On Windows this may include non-disk I/O.",
        "counter",
        &service,
        process.disk_read_bytes,
    );
    push_u64_metric(
        &mut out,
        "ores_otel_process_disk_write_bytes_total",
        "Cumulative bytes written by the process. On Windows this may include non-disk I/O.",
        "counter",
        &service,
        process.disk_write_bytes,
    );
    push_u64_metric(
        &mut out,
        "ores_otel_process_uptime_seconds",
        "Process uptime in seconds.",
        "gauge",
        &service,
        process.uptime_seconds,
    );

    for filesystem in &snapshot.filesystems {
        let target = escape_label(&filesystem.target);
        push_filesystem_metric(
            &mut out,
            "ores_otel_filesystem_total_bytes",
            "Filesystem total capacity in bytes.",
            &service,
            &target,
            filesystem.total_bytes as f64,
        );
        push_filesystem_metric(
            &mut out,
            "ores_otel_filesystem_used_bytes",
            "Filesystem used capacity in bytes.",
            &service,
            &target,
            filesystem.used_bytes as f64,
        );
        push_filesystem_metric(
            &mut out,
            "ores_otel_filesystem_available_bytes",
            "Filesystem space available to the current process in bytes.",
            &service,
            &target,
            filesystem.available_bytes as f64,
        );
        push_filesystem_metric(
            &mut out,
            "ores_otel_filesystem_utilization_ratio",
            "Filesystem utilization ratio.",
            &service,
            &target,
            filesystem.utilization_ratio,
        );
        push_filesystem_metric(
            &mut out,
            "ores_otel_filesystem_read_only",
            "Whether the filesystem is read-only.",
            &service,
            &target,
            f64::from(u8::from(filesystem.read_only)),
        );
    }
    out
}

fn push_u64_metric(
    out: &mut String,
    name: &str,
    help: &str,
    kind: &str,
    service: &str,
    value: u64,
) {
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} {kind}\n"));
    out.push_str(&format!("{name}{{service=\"{service}\"}} {value}\n"));
}

fn push_filesystem_metric(
    out: &mut String,
    name: &str,
    help: &str,
    service: &str,
    target: &str,
    value: f64,
) {
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} gauge\n"));
    out.push_str(&format!(
        "{name}{{service=\"{service}\",target=\"{target}\"}} {value}\n"
    ));
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::{
        render_prometheus, FilesystemResourceSnapshot, ProcessResourceSnapshot, ResourceSnapshot,
    };

    fn fixture() -> ResourceSnapshot {
        ResourceSnapshot {
            process: ProcessResourceSnapshot {
                cpu_time_seconds: 12.5,
                cpu_utilization_ratio: Some(0.25),
                cpu_sample_warmed: true,
                memory_usage_bytes: 256,
                memory_virtual_bytes: 1024,
                disk_read_bytes: 2048,
                disk_write_bytes: 4096,
                uptime_seconds: 60,
            },
            filesystems: vec![FilesystemResourceSnapshot {
                target: "fs0".into(),
                total_bytes: 10_000,
                used_bytes: 4_000,
                available_bytes: 6_000,
                utilization_ratio: 0.4,
                read_only: false,
            }],
        }
    }

    #[test]
    fn prometheus_projection_contains_resource_signals_without_paths() {
        let output = render_prometheus("service-a", &fixture());
        for metric in [
            "ores_otel_process_cpu_time_seconds",
            "ores_otel_process_memory_usage_bytes",
            "ores_otel_process_disk_read_bytes_total",
            "ores_otel_filesystem_available_bytes",
            "ores_otel_filesystem_utilization_ratio",
        ] {
            assert!(output.contains(metric), "missing {metric}: {output}");
        }
        assert!(output.contains("target=\"fs0\""));
        assert!(!output.contains("/var/"));
        assert!(!output.contains("/home/"));
    }

    #[test]
    fn prometheus_labels_are_escaped() {
        let output = render_prometheus("svc\"\\\n", &fixture());
        assert!(output.contains("service=\"svc\\\"\\\\\\n\""));
    }
}
