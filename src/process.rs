//! The `claude` process a hook runs for, and whether a process still runs.

use std::fs;

/// The process with this PID runs and started at `start`, so it is not a
/// new process with a reused PID. Elsewhere than Linux this cannot be
/// checked, and it says yes.
pub fn alive(pid: u32, start: u64) -> bool {
    #[cfg(target_os = "linux")]
    {
        Stat::read(pid).is_some_and(|s| s.start == start)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, start);
        true
    }
}

/// The Claude Code process this hook runs for. A watcher whose session has
/// ended must not keep the lock for its nickname.
#[cfg(target_os = "linux")]
pub struct Owner {
    /// The `claude` process.
    pub pid: u32,
    /// Its start time, so that a reused PID does not count as alive.
    pub start: u64,
}

#[cfg(target_os = "linux")]
impl Owner {
    /// `$CLAUDE_PID`, else the nearest ancestor named `claude`, else the
    /// parent process.
    pub fn find() -> Self {
        if let Some(pid) = std::env::var("CLAUDE_PID")
            .ok()
            .and_then(|p| p.parse().ok())
            && let Some(stat) = Stat::read(pid)
        {
            return Self {
                pid,
                start: stat.start,
            };
        }
        let parent = std::os::unix::process::parent_id();
        let mut pid = parent;
        for _ in 0..16 {
            let Some(stat) = Stat::read(pid) else { break };
            if stat.comm == "claude" {
                return Self {
                    pid,
                    start: stat.start,
                };
            }
            if stat.ppid <= 1 {
                break;
            }
            pid = stat.ppid;
        }
        let start = Stat::read(parent).map_or(0, |s| s.start);
        Self { pid: parent, start }
    }

    /// The process still runs, and it is the same process, not a new one
    /// with a reused PID.
    pub fn alive(&self) -> bool {
        Stat::read(self.pid).is_some_and(|s| s.start == self.start)
    }

    /// Claude Code says this session is in a turn (`busy`) or waits for a
    /// permission answer (`waiting`). It keeps one file per process in
    /// `~/.claude/sessions/<pid>.json`, with a `status` field. The file is
    /// internal and undocumented: the documented `claude agents --json` lists
    /// background sessions only, not ones started with `claude` in a
    /// terminal. If the file is missing or its format changes, this says no,
    /// so the watcher delivers as if the session were idle.
    pub fn in_turn(&self) -> bool {
        let dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
            Some(dir) => std::path::PathBuf::from(dir),
            None => match std::env::home_dir() {
                Some(home) => home.join(".claude"),
                None => return false,
            },
        };
        let Ok(text) = fs::read_to_string(dir.join("sessions").join(format!("{}.json", self.pid)))
        else {
            return false;
        };
        let status = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["status"].as_str().map(str::to_owned));
        matches!(status.as_deref(), Some("busy" | "waiting"))
    }
}

#[cfg(target_os = "linux")]
pub struct Stat {
    pub comm: String,
    pub ppid: u32,
    pub start: u64,
}

#[cfg(target_os = "linux")]
impl Stat {
    /// Parse `/proc/<pid>/stat`. The name is in parentheses and may contain
    /// spaces, so the other fields are counted from the last `)`.
    pub fn read(pid: u32) -> Option<Self> {
        let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let open = text.find('(')?;
        let close = text.rfind(')')?;
        let fields: Vec<&str> = text[close + 1..].split_whitespace().collect();
        Some(Self {
            comm: text[open + 1..close].to_owned(),
            // Fields 4 and 22 of stat(5); `fields[0]` is field 3.
            ppid: fields.get(1)?.parse().ok()?,
            start: fields.get(19)?.parse().ok()?,
        })
    }
}

/// Elsewhere goniec cannot see the owner's process, so the watcher waits
/// until Claude Code ends it.
#[cfg(not(target_os = "linux"))]
pub struct Owner {
    /// The `claude` process, from `$CLAUDE_PID`, or 0 when unknown.
    pub pid: u32,
    /// Not known here; `alive` does not check it.
    pub start: u64,
}

#[cfg(not(target_os = "linux"))]
impl Owner {
    pub fn find() -> Self {
        let pid = std::env::var("CLAUDE_PID")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        Owner { pid, start: 0 }
    }

    pub fn alive(&self) -> bool {
        true
    }

    pub fn in_turn(&self) -> bool {
        false
    }
}
