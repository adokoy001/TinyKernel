//! Per-domain resource limits and accounting: tasks, physical frames,
//! files on disk, and CPU share. Access control (`mac`) decides whether a
//! domain may do something at all; this decides whether it still has the
//! resources to. Pure logic without hardware access, shared by the kernel
//! and host tests.

use crate::mac::Domain;

/// Timer ticks per CPU accounting window (1 s at 100 Hz).
pub const CPU_WINDOW: u32 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Spawned tasks alive at once (the shell and idle are not counted).
    pub tasks: u32,
    /// 4 KiB frames held: `alloc` frames plus 4 per task stack.
    pub frames: u32,
    /// Files on disk with this label.
    pub files: u32,
    /// Percent of each CPU window this domain's spawned tasks may use
    /// while tasks of another domain are waiting to run.
    pub cpu_percent: u32,
}

/// The limits, as fixed as the access policy.
pub const fn limits(domain: Domain) -> Limits {
    match domain {
        Domain::Kernel => Limits { tasks: 0, frames: 0, files: 0, cpu_percent: 100 },
        Domain::Admin => Limits { tasks: 6, frames: 4096, files: 24, cpu_percent: 70 },
        Domain::User => Limits { tasks: 3, frames: 24, files: 8, cpu_percent: 30 },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    Tasks,
    Frames,
    Files,
}

impl Resource {
    pub fn name(self) -> &'static str {
        match self {
            Resource::Tasks => "tasks",
            Resource::Frames => "frames",
            Resource::Files => "files",
        }
    }
}

/// A request that would exceed a limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exceeded {
    pub resource: Resource,
    pub used: u32,
    pub limit: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub tasks: u32,
    pub frames: u32,
    /// Ticks used by spawned tasks in the current and the last full window.
    pub cpu_ticks: u32,
    pub last_window_ticks: u32,
}

impl Usage {
    const ZERO: Usage = Usage { tasks: 0, frames: 0, cpu_ticks: 0, last_window_ticks: 0 };
}

/// Usage of every domain. Files are counted from the disk's file table.
pub struct Accounts {
    usage: [Usage; 3],
    window_tick: u32,
}

impl Accounts {
    pub const fn new() -> Self {
        Self { usage: [Usage::ZERO; 3], window_tick: 0 }
    }

    pub fn usage(&self, domain: Domain) -> Usage {
        self.usage[domain.index()]
    }

    /// Reserve `tasks` task slots and `frames` frames together, or nothing.
    pub fn charge(&mut self, domain: Domain, tasks: u32, frames: u32) -> Result<(), Exceeded> {
        let limit = limits(domain);
        let usage = &mut self.usage[domain.index()];
        if usage.tasks + tasks > limit.tasks {
            return Err(Exceeded { resource: Resource::Tasks, used: usage.tasks, limit: limit.tasks });
        }
        if usage.frames + frames > limit.frames {
            return Err(Exceeded { resource: Resource::Frames, used: usage.frames, limit: limit.frames });
        }
        usage.tasks += tasks;
        usage.frames += frames;
        Ok(())
    }

    pub fn release(&mut self, domain: Domain, tasks: u32, frames: u32) {
        let usage = &mut self.usage[domain.index()];
        usage.tasks = usage.tasks.saturating_sub(tasks);
        usage.frames = usage.frames.saturating_sub(frames);
    }

    /// One timer tick spent by a spawned task of `domain` (None: the shell
    /// or idle, which are never throttled and only advance the window).
    pub fn tick(&mut self, domain: Option<Domain>) {
        if let Some(domain) = domain {
            self.usage[domain.index()].cpu_ticks += 1;
        }
        self.window_tick += 1;
        if self.window_tick == CPU_WINDOW {
            self.window_tick = 0;
            for usage in self.usage.iter_mut() {
                usage.last_window_ticks = usage.cpu_ticks;
                usage.cpu_ticks = 0;
            }
        }
    }

    /// Whether `domain` has used its CPU share of the current window.
    pub fn over_cpu_share(&self, domain: Domain) -> bool {
        self.usage[domain.index()].cpu_ticks * 100 >= limits(domain).cpu_percent * CPU_WINDOW
    }
}

/// Check a file count against the file limit.
pub fn check_files(domain: Domain, used: u32) -> Result<(), Exceeded> {
    let limit = limits(domain).files;
    if used >= limit {
        return Err(Exceeded { resource: Resource::Files, used, limit });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{check_files, limits, Accounts, Exceeded, Resource, CPU_WINDOW};
    use crate::mac::Domain::*;

    #[test]
    fn charges_are_all_or_nothing_and_released() {
        let mut accounts = Accounts::new();
        let user = limits(User);
        for _ in 0..user.tasks {
            accounts.charge(User, 1, 4).unwrap();
        }
        assert_eq!(accounts.charge(User, 1, 4),
                   Err(Exceeded { resource: Resource::Tasks, used: user.tasks, limit: user.tasks }));
        assert_eq!(accounts.usage(User).frames, 4 * user.tasks);
        accounts.release(User, 1, 4);
        // Frames: fill up to the limit, then a stack no longer fits.
        accounts.charge(User, 0, user.frames - accounts.usage(User).frames - 2).unwrap();
        assert_eq!(accounts.charge(User, 1, 4).unwrap_err().resource, Resource::Frames);
        assert_eq!(accounts.usage(User).tasks, user.tasks - 1, "a refused charge changes nothing");
        // Domains are accounted separately.
        accounts.charge(Admin, 1, 4).unwrap();
        assert_eq!(accounts.usage(Admin).tasks, 1);
        accounts.release(Admin, 5, 500);
        assert_eq!(accounts.usage(Admin).frames, 0, "release saturates");
    }

    #[test]
    fn kernel_domain_gets_no_resources() {
        let mut accounts = Accounts::new();
        assert!(accounts.charge(Kernel, 1, 0).is_err());
        assert!(accounts.charge(Kernel, 0, 1).is_err());
        assert!(check_files(Kernel, 0).is_err());
    }

    #[test]
    fn file_limit() {
        assert!(check_files(User, limits(User).files - 1).is_ok());
        assert_eq!(check_files(User, limits(User).files).unwrap_err().resource, Resource::Files);
    }

    #[test]
    fn cpu_share_resets_every_window() {
        let mut accounts = Accounts::new();
        let share = limits(User).cpu_percent * CPU_WINDOW / 100;
        for _ in 0..share - 1 {
            accounts.tick(Some(User));
        }
        assert!(!accounts.over_cpu_share(User));
        accounts.tick(Some(User));
        assert!(accounts.over_cpu_share(User));
        assert!(!accounts.over_cpu_share(Admin));
        for _ in share..CPU_WINDOW {
            accounts.tick(None);
        }
        assert!(!accounts.over_cpu_share(User), "a new window starts");
        assert_eq!(accounts.usage(User).last_window_ticks, share);
    }
}
