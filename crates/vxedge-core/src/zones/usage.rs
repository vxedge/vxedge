//! **What a node costs the host** (`ARCHITECTURE.md` §12): its CPU time and memory, read where
//! the kernel keeps them, so the daemon's exporter can name them by entity.
//!
//! A container node is its payload's cgroup: everything the container runs, children included.
//! A VM or capsule node is its VMM or runner process. Those run in the daemon's own cgroup, so
//! their cgroup would count the daemon and every other guest; the process's own `/proc` counters
//! (every thread) are the node.

use std::path::{Path, PathBuf};

/// Where a node's usage is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A cgroup v2 directory under `/sys/fs/cgroup`.
    Cgroup(PathBuf),
    /// One process: a VMM or a runner.
    Process(i32),
}

impl Source {
    /// How the source is labelled: `cgroup` or `process`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Cgroup(_) => "cgroup",
            Self::Process(_) => "process",
        }
    }
}

/// One reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    /// CPU time used since the source started, user and system.
    pub cpu_seconds: f64,
    /// Memory in use now: a cgroup's `memory.current`, a process's resident set.
    pub memory_bytes: u64,
}

/// Clock ticks per second in `/proc/<pid>/stat`. `USER_HZ` is part of the Linux ABI and is 100 on
/// every architecture vxedge builds for.
const USER_HZ: f64 = 100.0;

/// Read `source` now, or `None` when it is gone.
pub fn read(source: &Source) -> Option<Usage> {
    match source {
        Source::Cgroup(dir) => {
            let cpu = parse_cpu_stat(&std::fs::read_to_string(dir.join("cpu.stat")).ok()?)?;
            let mem = std::fs::read_to_string(dir.join("memory.current"))
                .ok()?
                .trim()
                .parse()
                .ok()?;
            Some(Usage {
                cpu_seconds: cpu as f64 / 1e6,
                memory_bytes: mem,
            })
        }
        Source::Process(pid) => {
            let proc = Path::new("/proc").join(pid.to_string());
            let (utime, stime) =
                parse_proc_stat(&std::fs::read_to_string(proc.join("stat")).ok()?)?;
            let rss = parse_vm_rss(&std::fs::read_to_string(proc.join("status")).ok()?)?;
            Some(Usage {
                cpu_seconds: (utime + stime) as f64 / USER_HZ,
                memory_bytes: rss,
            })
        }
    }
}

/// The cgroup v2 directory process `pid` is in.
pub fn cgroup_of(pid: i32) -> Option<PathBuf> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    cgroup_dir(&text)
}

fn cgroup_dir(text: &str) -> Option<PathBuf> {
    let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(Path::new("/sys/fs/cgroup").join(rel.trim().trim_start_matches('/')))
}

/// `usage_usec` from a cgroup's `cpu.stat`.
fn parse_cpu_stat(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
}

/// `utime` and `stime`, fields 14 and 15 of `/proc/<pid>/stat`. The command name in field 2 may
/// hold spaces and parentheses, so fields are counted from its last `)`.
fn parse_proc_stat(text: &str) -> Option<(u64, u64)> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace().skip(11);
    Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
}

/// `VmRSS` from `/proc/<pid>/status`, in bytes.
fn parse_vm_rss(text: &str) -> Option<u64> {
    let kb: u64 = text
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_counts_fields_after_the_command_name() {
        let stat = "4242 (qemu (vx) x) S 1 4242 4242 0 -1 4194560 100 0 0 0 250 75 0 0 20 0 3 0";
        assert_eq!(parse_proc_stat(stat), Some((250, 75)));
        assert_eq!(parse_proc_stat("garbage"), None);
    }

    #[test]
    fn status_and_cgroup_files_parse() {
        assert_eq!(
            parse_vm_rss("Name:\tqemu\nVmRSS:\t  2048 kB\nThreads:\t3\n"),
            Some(2 * 1024 * 1024)
        );
        assert_eq!(parse_vm_rss("Name:\tkthread\n"), None);
        assert_eq!(
            parse_cpu_stat("usage_usec 5495\nuser_usec 915\nsystem_usec 4579\n"),
            Some(5495)
        );
        assert_eq!(
            cgroup_dir("0::/user.slice/libpod-62d3.scope/container\n"),
            Some(PathBuf::from(
                "/sys/fs/cgroup/user.slice/libpod-62d3.scope/container"
            ))
        );
        assert_eq!(cgroup_dir("1:name=systemd:/x\n"), None);
    }

    #[test]
    fn this_process_reads_as_a_source() {
        let u = read(&Source::Process(std::process::id() as i32)).expect("own /proc");
        assert!(u.memory_bytes > 0);
        assert!(read(&Source::Process(i32::MAX)).is_none());
    }
}
