//! Bounded, read-only hardware probes. Unsupported observations remain absent.

use phonton_types::local::{GpuSnapshot, HardwareSnapshot};
use std::time::Duration;
use tokio::process::Command;

async fn output(program: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new(program);
    command.args(args).kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let result = tokio::time::timeout(Duration::from_secs(12), command.output())
        .await
        .ok()?
        .ok()?;
    result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

/// Inspect RAM, CPU and NVIDIA memory without starting an inference runtime.
pub async fn detect() -> HardwareSnapshot {
    let mut hardware = HardwareSnapshot {
        logical_cpus: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        ..Default::default()
    };
    #[cfg(windows)]
    {
        let native_ram = windows_ram();
        // Native memory needs no subprocess. If it fails, keep the RAM-only
        // fallback independent of the optional CPU-name query.
        let cim_ram = if native_ram.is_none() {
            output("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", "$c=Get-CimInstance Win32_ComputerSystem; $o=Get-CimInstance Win32_OperatingSystem; @{total=[uint64]$c.TotalPhysicalMemory;available=([uint64]$o.FreePhysicalMemory*1024)} | ConvertTo-Json -Compress"]).await
        } else {
            None
        };
        // CPU naming is optional. A slow CIM call must not delay first-use
        // memory admission for the full hardware-probe timeout.
        let cim_cpu = tokio::time::timeout(
            Duration::from_secs(3),
            output("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", "$p=Get-CimInstance Win32_Processor; @{cpu=($p.Name -join ', ')} | ConvertTo-Json -Compress"]),
        )
        .await
        .ok()
        .flatten();
        apply_windows_observations(
            &mut hardware,
            native_ram,
            cim_ram.as_deref(),
            cim_cpu.as_deref(),
        );
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(mem) = tokio::fs::read_to_string("/proc/meminfo").await {
            hardware.ram_total_bytes = mem_value(&mem, "MemTotal:");
            hardware.ram_available_bytes = mem_value(&mem, "MemAvailable:");
        }
        if let Ok(cpu) = tokio::fs::read_to_string("/proc/cpuinfo").await {
            hardware.cpu = cpu
                .lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().into());
        }
    }
    #[cfg(target_os = "macos")]
    {
        hardware.cpu = output("sysctl", &["-n", "machdep.cpu.brand_string"]).await;
        hardware.ram_total_bytes = output("sysctl", &["-n", "hw.memsize"])
            .await
            .and_then(|s| s.parse().ok());
        if let (Some(total), Some(statistics)) =
            (hardware.ram_total_bytes, output("vm_stat", &[]).await)
        {
            hardware.ram_available_bytes = macos_available_ram(&statistics, total);
            if hardware.ram_available_bytes.is_some() {
                hardware.warnings.push("macOS available RAM includes reclaimable inactive and speculative pages. This is a point-in-time estimate, not a reservation; loading a model can still cause paging.".into());
            }
        }
    }
    if let Some(gpus) = output(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total,memory.free",
            "--format=csv,noheader,nounits",
        ],
    )
    .await
    {
        hardware.gpus = parse_nvidia(&gpus);
    }
    if hardware.ram_available_bytes.is_none() {
        hardware
            .warnings
            .push("Available RAM could not be measured; model fit is unknown.".into());
    }
    if hardware.gpus.is_empty() {
        hardware.warnings.push("No NVIDIA memory measurement. Other accelerators may exist; no GPU capacity is assumed.".into());
    }
    hardware
}

#[cfg(windows)]
fn windows_ram() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut memory = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: memory is initialized and its length is set to the exact Win32
    // structure size; the mutable pointer remains valid for this call.
    let measured = unsafe { GlobalMemoryStatusEx(&mut memory) } != 0;
    (measured && memory.ullTotalPhys > 0 && memory.ullAvailPhys <= memory.ullTotalPhys)
        .then_some((memory.ullTotalPhys, memory.ullAvailPhys))
}

#[cfg(windows)]
fn apply_windows_observations(
    hardware: &mut HardwareSnapshot,
    native_ram: Option<(u64, u64)>,
    cim_ram_json: Option<&str>,
    cim_cpu_json: Option<&str>,
) {
    let cim_cpu =
        cim_cpu_json.and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
    if let Some(cpu) = cim_cpu
        .as_ref()
        .and_then(|data| data["cpu"].as_str())
        .map(str::trim)
        .filter(|cpu| !cpu.is_empty())
    {
        hardware.cpu = Some(cpu.to_owned());
    }
    let cim_ram = cim_ram_json
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .and_then(|data| {
            let total = data["total"].as_u64()?;
            let available = data["available"].as_u64()?;
            (total > 0 && available <= total).then_some((total, available))
        });
    if let Some((total, available)) = native_ram
        .filter(|(total, available)| *total > 0 && available <= total)
        .or(cim_ram)
    {
        hardware.ram_total_bytes = Some(total);
        hardware.ram_available_bytes = Some(available);
    }
}

#[cfg(target_os = "linux")]
fn mem_value(text: &str, name: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.starts_with(name))?
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}

#[cfg(any(test, target_os = "macos"))]
fn macos_available_ram(text: &str, total_bytes: u64) -> Option<u64> {
    let header = text.lines().next()?.trim();
    if !header.starts_with("Mach Virtual Memory Statistics:") {
        return None;
    }
    let page_size = header
        .split_once("page size of ")?
        .1
        .split_once(" bytes")?
        .0
        .parse::<u64>()
        .ok()?;
    if !page_size.is_power_of_two() || !(4096..=65536).contains(&page_size) {
        return None;
    }
    let pages = |label: &str| {
        text.lines()
            .find(|line| line.trim_start().starts_with(label))?
            .trim_start()
            .strip_prefix(label)?
            .trim()
            .strip_suffix('.')?
            .parse::<u64>()
            .ok()
    };
    // vm_stat prints free pages excluding speculative pages. Inactive pages
    // can be reclaimed, but may require paging; the fit estimate retains its
    // separate host reserve and never treats this observation as a guarantee.
    let count = pages("Pages free:")?
        .checked_add(pages("Pages inactive:")?)?
        .checked_add(pages("Pages speculative:")?)?;
    let available = count.checked_mul(page_size)?;
    (available <= total_bytes).then_some(available)
}

fn parse_nvidia(text: &str) -> Vec<GpuSnapshot> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.rsplitn(3, ',');
            let available_bytes = parts
                .next()?
                .trim()
                .parse::<u64>()
                .ok()?
                .checked_mul(1024 * 1024)?;
            let total_bytes = parts
                .next()?
                .trim()
                .parse::<u64>()
                .ok()?
                .checked_mul(1024 * 1024)?;
            let name = parts.next()?.trim().to_owned();
            (available_bytes <= total_bytes).then_some(GpuSnapshot {
                name,
                total_bytes,
                available_bytes,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::local::FitStatus;

    #[test]
    fn macos_vm_stat_estimate_uses_reported_page_size_and_reclaimable_pages() {
        let statistics = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:                          100000.\nPages active:                        300000.\nPages inactive:                      200000.\nPages speculative:                    20000.\n";
        let total = 16u64 << 30;
        let available = macos_available_ram(statistics, total);
        assert_eq!(available, Some(320000 * 16384));
        let hardware = HardwareSnapshot {
            ram_total_bytes: Some(total),
            ram_available_bytes: available,
            ..Default::default()
        };
        assert_eq!(
            crate::estimate_fit(1u64 << 30, &hardware).status,
            FitStatus::CpuOrOffload
        );
    }

    #[test]
    fn macos_vm_stat_estimate_rejects_missing_malformed_or_impossible_pages() {
        let total = 16u64 << 30;
        let valid = "Mach Virtual Memory Statistics: (page size of 4096 bytes)\nPages free: 100000.\nPages inactive: 200000.\nPages speculative: 20000.\n";
        assert_eq!(macos_available_ram(valid, total), Some(320000 * 4096));
        assert_eq!(
            macos_available_ram(&valid.replace("Pages inactive:", "Absent:"), total),
            None
        );
        assert_eq!(
            macos_available_ram(&valid.replace("100000.", "unknown."), total),
            None
        );
        assert_eq!(
            macos_available_ram(&valid.replace("4096", "0"), total),
            None
        );
        assert_eq!(
            macos_available_ram(&valid.replace("100000.", "18446744073709551615."), total),
            None
        );
        assert_eq!(macos_available_ram(valid, 1), None);
    }

    #[test]
    fn ignores_unsupported_gpu_measurements() {
        let gpus = parse_nvidia("GPU, 6141, 5800\nOther, [N/A], [N/A]\nBroken, 10, 12");
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].available_bytes, 5800 * 1024 * 1024);
    }

    #[cfg(windows)]
    #[test]
    fn native_ram_keeps_model_fit_available_when_cim_fails() {
        let mut hardware = HardwareSnapshot::default();
        apply_windows_observations(&mut hardware, Some((16u64 << 30, 8u64 << 30)), None, None);
        assert_eq!(hardware.ram_total_bytes, Some(16u64 << 30));
        assert_eq!(hardware.ram_available_bytes, Some(8u64 << 30));
        assert_eq!(
            crate::estimate_fit(1u64 << 30, &hardware).status,
            FitStatus::CpuOrOffload
        );

        apply_windows_observations(
            &mut hardware,
            Some((16u64 << 30, 8u64 << 30)),
            None,
            Some("not json"),
        );
        assert_eq!(hardware.ram_available_bytes, Some(8u64 << 30));
    }

    #[cfg(windows)]
    #[test]
    fn native_ram_is_authoritative_and_cim_can_fall_back() {
        let cim_ram = r#"{"total":34359738368,"available":3221225472}"#;
        let cim_cpu = r#"{"cpu":"Fixture CPU"}"#;
        let mut hardware = HardwareSnapshot::default();
        apply_windows_observations(
            &mut hardware,
            Some((16u64 << 30, 8u64 << 30)),
            Some(cim_ram),
            Some(cim_cpu),
        );
        assert_eq!(hardware.cpu.as_deref(), Some("Fixture CPU"));
        assert_eq!(hardware.ram_total_bytes, Some(16u64 << 30));
        assert_eq!(hardware.ram_available_bytes, Some(8u64 << 30));

        apply_windows_observations(&mut hardware, None, Some(cim_ram), None);
        assert_eq!(hardware.ram_total_bytes, Some(32u64 << 30));
        assert_eq!(hardware.ram_available_bytes, Some(3u64 << 30));
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_ram_api_reports_a_plausible_observation() {
        let (total, available) =
            windows_ram().expect("Windows physical memory should be measurable");
        assert!(total > 0 && available <= total);
    }
}
