//! Host safety for a run: where it may write, how many guests may be alive,
//! and which log lines mean the run proves nothing.
//!
//! A guest is a multi-gigabyte writer. These checks exist because a full
//! filesystem does not fail loudly: the emulator's disk writes error out, the
//! guest sees a media error, and the run "fails" for a reason unrelated to
//! the emulator.

use std::path::{Path, PathBuf};

/// A run whose logs contain one of these is invalid, not failed: the host,
/// not the emulator, broke it.
pub const INVALIDATING: &[&str] = &["Media error: Write error"];

/// The first line of `text` that invalidates the run.
pub fn invalidating_line(text: &str) -> Option<&str> {
    text.lines().find(|l| INVALIDATING.iter().any(|p| l.contains(p)))
}

/// The run root must be absolute and outside the system temporary directory,
/// which on many hosts is a small tmpfs.
pub fn check_run_root(root: &Path, system_tmp: &Path) -> Result<(), String> {
    if !root.is_absolute() {
        return Err(format!("run root {} is not absolute", root.display()));
    }
    if root.starts_with(system_tmp) {
        return Err(format!(
            "run root {} is inside the system temporary directory {}",
            root.display(),
            system_tmp.display()
        ));
    }
    Ok(())
}

pub fn check_free_space(free_bytes: u64, min_gb: u64) -> Result<(), String> {
    let free_gb = free_bytes >> 30;
    if free_gb < min_gb {
        return Err(format!("{free_gb} GB free, at least {min_gb} GB required"));
    }
    Ok(())
}

/// Free bytes on the filesystem holding `path`, for unprivileged writers.
#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: statvfs fills the zeroed struct for a valid NUL-terminated path.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(format!("statvfs {}: {}", path.display(), std::io::Error::last_os_error()));
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

/// The 1-minute load average and the CPU count, recorded with every result so
/// a slow boot can be read against the host it ran on.
#[cfg(unix)]
pub fn host_load() -> (f64, usize) {
    let mut l = [0f64; 3];
    // SAFETY: getloadavg writes at most 3 doubles.
    let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    (if n > 0 { l[0] } else { 0.0 }, cpus)
}

/// Timeouts stretch when the host has more runnable work than CPUs, since the
/// guest's threads then wait for the host scheduler. Never shrinks them, and
/// stops at 4x: a host that slow wants a human, not a longer wait.
pub fn auto_scale(load1: f64, cpus: usize) -> f64 {
    (load1 / cpus.max(1) as f64).clamp(1.0, 4.0)
}

/// Guests the harness may have alive at once, counted from the pid files that
/// each run directory under the run root holds.
pub const PID_FILE: &str = "harness.pid";

#[cfg(unix)]
pub fn live_guests(run_root: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(run_root) else { return 0 };
    rd.flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join(PID_FILE)).ok())
        .filter_map(|t| t.trim().parse::<i32>().ok())
        .filter(|&pid| pid_alive(pid))
        .count()
}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn check_guest_slots(live: usize, max: usize) -> Result<(), String> {
    if live >= max {
        return Err(format!("{live} guests already running under the run root, at most {max} allowed"));
    }
    Ok(())
}

/// A fresh run directory name under `root`.
pub fn run_dir(root: &Path, scenario: &str, unix_secs: u64, pid: u32) -> PathBuf {
    root.join(format!("accept-{scenario}-{unix_secs}-{pid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_write_error_invalidates_the_run() {
        let log = "boot ok\nMedia error: Write error (asc=0xc) on scsi1\nlogin:";
        assert_eq!(invalidating_line(log), Some("Media error: Write error (asc=0xc) on scsi1"));
        assert_eq!(invalidating_line("Media error: Read error\nPANIC"), None);
    }

    #[test]
    fn the_run_root_must_be_absolute_and_outside_the_system_tmp() {
        let tmp = Path::new("/tmp");
        assert!(check_run_root(Path::new("/scratch/runs"), tmp).is_ok());
        assert!(check_run_root(Path::new("runs"), tmp).unwrap_err().contains("absolute"));
        assert!(check_run_root(Path::new("/tmp/runs"), tmp).unwrap_err().contains("temporary"));
        // A sibling that merely shares the prefix is not inside it.
        assert!(check_run_root(Path::new("/tmpfoo/runs"), tmp).is_ok());
    }

    #[test]
    fn free_space_is_compared_in_whole_gigabytes() {
        let gb = 1u64 << 30;
        assert!(check_free_space(150 * gb, 150).is_ok());
        assert!(check_free_space(150 * gb - 1, 150).is_err());
    }

    #[test]
    fn the_guest_limit_counts_live_guests_only() {
        assert!(check_guest_slots(1, 2).is_ok());
        assert!(check_guest_slots(2, 2).is_err());
    }

    #[test]
    fn timeouts_stretch_with_load_but_never_shrink() {
        assert_eq!(auto_scale(0.5, 8), 1.0);
        assert_eq!(auto_scale(16.0, 8), 2.0);
        assert_eq!(auto_scale(800.0, 8), 4.0);
        assert_eq!(auto_scale(3.0, 0), 3.0);
    }

    #[cfg(unix)]
    #[test]
    fn live_guests_ignores_stale_and_unreadable_pid_files() {
        let root = std::env::temp_dir().join(format!("iris-accept-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (dir, pid) in [("mine", std::process::id().to_string()), ("junk", "x".into()),
                           ("dead", "2147483646".into())] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join(PID_FILE), pid).unwrap();
        }
        std::fs::create_dir_all(root.join("empty")).unwrap();
        assert_eq!(live_guests(&root), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn the_free_space_probe_answers_for_a_real_directory() {
        let here = std::env::current_dir().unwrap();
        assert!(free_bytes(&here).unwrap() > 0);
        assert!(free_bytes(Path::new("/nonexistent-iris-accept")).is_err());
    }

    #[test]
    fn run_directories_are_named_by_scenario_time_and_pid() {
        assert_eq!(
            run_dir(Path::new("/r"), "o2", 100, 7),
            PathBuf::from("/r/accept-o2-100-7")
        );
    }
}
