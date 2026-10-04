//! The shell's shared operation catalog: resolution, help and completion.
//!
//! Metadata describes effects and results; enforcement remains in the
//! executor and the kernel's mandatory-access checks. Aliases never add a
//! second implementation or bypass the canonical operation.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Operation {
    Help, About, Status, Memory, Clear, Halt, Reboot, Uptime, Alloc, Tasks,
    Security, Resources, Audit, DropToUser, Disk, Network, Ping, Format,
    List, Cat, Write, Append, Remove, Echo, Calc, Sleep, Fault, Free,
    Spawn, Kill, Ops, Vars, Let, Unset, History, Run, Plan, Show, Apply,
    ProcessPrograms, ProcessList, ProcessRun, ProcessExec, ProcessInstall, ProcessWait, ProcessOutput,
    ProcessMemory, ProcessPause, ProcessResume,
    StorageStatus, StorageCheck, StorageSync, Rename, Truncate, Upgrade,
    Where, Select, Sort, Take, Count, Json, Save,
}

pub const OPERATION_COUNT: usize = Operation::Save as usize + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect { ReadOnly, Write, Control }

impl Effect {
    pub const fn name(self) -> &'static str {
        match self { Self::ReadOnly => "read", Self::Write => "write", Self::Control => "control" }
    }
}

/// Names of source result schemas. Transform output schemas are validated
/// separately against their input's actual columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schema {
    Unit, Text, Number, Bytes, Status, Memory, Tasks, Security, Resources,
    Audit, Files, Network, Operations, Variables, History, Records,
}

impl Schema {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unit => "unit", Self::Text => "text", Self::Number => "number",
            Self::Bytes => "bytes", Self::Status => "status", Self::Memory => "memory",
            Self::Tasks => "tasks", Self::Security => "security", Self::Resources => "resources",
            Self::Audit => "audit", Self::Files => "files", Self::Network => "network",
            Self::Operations => "operations", Self::Variables => "variables",
            Self::History => "history", Self::Records => "records",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Metadata {
    pub operation: Operation,
    /// Canonical command spelling, including a subcommand when applicable.
    pub name: &'static str,
    /// Single-word legacy spellings accepted by the same executor.
    pub aliases: &'static [&'static str],
    pub usage: &'static str,
    pub description: &'static str,
    pub example: &'static str,
    pub effect: Effect,
    pub schema: Schema,
    pub permission: &'static str,
}

macro_rules! entry {
    ($op:ident, $name:literal, [$($alias:literal),*], $usage:literal, $description:literal, $example:literal, $effect:ident, $schema:ident, $permission:literal) => {
        Metadata { operation: Operation::$op, name: $name, aliases: &[$($alias),*],
            usage: $usage, description: $description, example: $example,
            effect: Effect::$effect, schema: Schema::$schema, permission: $permission }
    };
}

/// One catalog, in Operation discriminant order. Resolution, generated help,
/// the `ops` source and completion should all consume this table.
pub static OPERATIONS: [Metadata; OPERATION_COUNT] = [
    entry!(Help, "help", [], "help [OPERATION]", "List commands or inspect one command's contract.", "help task list", ReadOnly, Text, "all domains"),
    entry!(About, "about", [], "about", "Describe the kernel and shell.", "about", ReadOnly, Text, "all domains"),
    entry!(Status, "status", [], "status", "Read the preceding command's result code, operation and message.", "status | json", ReadOnly, Status, "all domains"),
    entry!(Memory, "mem", ["memory"], "mem", "Read physical-memory usage.", "mem | json", ReadOnly, Memory, "all domains"),
    entry!(Clear, "clear", [], "clear", "Clear the terminal display.", "clear", Control, Unit, "all domains"),
    entry!(Halt, "halt", [], "halt", "Stop the processor until reset.", "help halt", Control, Unit, "admin only"),
    entry!(Reboot, "reboot", [], "reboot", "Restart the machine.", "help reboot", Control, Unit, "admin only"),
    entry!(Uptime, "uptime", [], "uptime", "Read elapsed milliseconds since boot.", "uptime", ReadOnly, Number, "all domains"),
    entry!(Alloc, "alloc", [], "alloc", "Allocate and register one physical frame.", "alloc", Write, Number, "current domain's memory quota"),
    entry!(Tasks, "task list", ["ps", "tasks"], "task list", "Read task identities, state and ownership.", "task list | select pid state", ReadOnly, Tasks, "all domains"),
    entry!(Security, "sec", ["security"], "sec", "Read the current domain and mandatory-access policy.", "sec", ReadOnly, Security, "all domains"),
    entry!(Resources, "top", ["resources"], "top", "Read domain resource use and quotas.", "top | json", ReadOnly, Resources, "all domains"),
    entry!(Audit, "audit", [], "audit", "Read the bounded log of denied operations.", "audit | json", ReadOnly, Audit, "admin only"),
    entry!(DropToUser, "drop", [], "drop", "Irreversibly lower this shell to the user domain until reboot.", "help drop", Control, Unit, "admin to user only"),
    entry!(Disk, "disk", [], "disk", "Read block-device and filesystem status.", "disk", ReadOnly, Records, "all domains"),
    entry!(Network, "net status", ["net"], "net status", "Read NIC, address and neighbor status.", "net status | json", ReadOnly, Network, "all domains"),
    entry!(Ping, "net ping", ["ping"], "net ping IP [--count 1-10] [--timeout 1-5000]", "Send bounded ICMP echo requests; Ctrl-C cancels.", "net ping 10.0.2.2 --count 1", Control, Records, "admin only, checked before any network transmission"),
    entry!(Format, "file format", ["format"], "file format", "Create an empty filesystem, replacing its current contents.", "help file format", Write, Unit, "admin only"),
    entry!(List, "file list", ["ls"], "file list", "Read file names, byte lengths and owners.", "file list | sort name", ReadOnly, Files, "all domains"),
    entry!(Cat, "file read", ["cat"], "file read NAME", "Read one file as literal text.", "file read notes", ReadOnly, Text, "admin: admin/user files; user: user files"),
    entry!(Write, "file write", ["write"], "file write NAME [TEXT]", "Create or replace one file with literal text.", "plan file write notes \"hello world\"", Write, Unit, "file ownership and current domain's storage quota"),
    entry!(Append, "file append", ["append"], "file append NAME TEXT", "Append literal text to an existing or new file.", "file append notes \"\\nnext line\"", Write, Unit, "file ownership and current domain's storage quota"),
    entry!(Remove, "file remove", ["rm"], "file remove NAME", "Remove one named file.", "plan file remove notes", Write, Unit, "admin: admin/user files; user: user files"),
    entry!(Echo, "echo", [], "echo [TEXT ...]", "Emit decoded words as one literal text result.", "echo \"pipe | is data\"", ReadOnly, Text, "all domains"),
    entry!(Calc, "calc", [], "calc I64 (+|-|*|/) I64", "Perform checked signed-integer arithmetic.", "calc 6 * 7", ReadOnly, Number, "all domains"),
    entry!(Sleep, "sleep", [], "sleep MS", "Sleep for 0-60000 milliseconds while other tasks run.", "sleep 100", Control, Unit, "all domains"),
    entry!(Fault, "fault", [], "fault bp|de|ud|gp|pf|df|null|ro|nx", "Raise a diagnostic CPU exception; some modes stop the system.", "help fault", Control, Unit, "admin only"),
    entry!(Free, "free", [], "free ADDR", "Release a registered physical frame (decimal or 0x hex address).", "free 0x100000", Write, Unit, "admin: admin/user frames; user: user frames"),
    entry!(Spawn, "task spawn", ["spawn"], "task spawn spin|beat|once", "Start one bounded demonstration task.", "task spawn once", Write, Number, "current domain's task quota"),
    entry!(Kill, "task kill", ["kill"], "task kill PID", "Stop an owned non-kernel demonstration task.", "task kill 3", Write, Unit, "admin: admin/user tasks; user: user tasks"),
    entry!(Ops, "ops", [], "ops", "Read operation names, contracts, effects and result schemas.", "ops | select name effect", ReadOnly, Operations, "all domains"),
    entry!(Vars, "vars", [], "vars", "Read the shell's bounded variable store.", "vars | json", ReadOnly, Variables, "all domains"),
    entry!(Let, "let", [], "let NAME VALUE", "Set one literal variable; expansion never reparses its value.", "let TARGET 10.0.2.2", Write, Unit, "current shell only"),
    entry!(Unset, "unset", [], "unset NAME", "Remove one shell variable.", "unset TARGET", Write, Unit, "current shell only"),
    entry!(History, "history", [], "history", "Read bounded recent command lines.", "history | json", ReadOnly, History, "current shell only"),
    entry!(Run, "run", [], "run NAME", "Run a file as one pipeline per line; stop at the first failure.", "run startup", Control, Unit, "readable file; every contained operation keeps its own checks"),
    entry!(Plan, "plan", [], "plan file write|append|remove NAME [TEXT]", "Hold one literal file mutation with its creator and verified target identity.", "plan file write notes \"hello\"", Control, Text, "file read and mutation permissions; apply rechecks them"),
    entry!(Show, "show", [], "show", "Show the currently held plan.", "show", ReadOnly, Text, "current shell only"),
    entry!(Apply, "apply", [], "apply [ID]", "Consume the held plan once; recheck its domain, revision, target and permissions.", "apply", Control, Unit, "file read and mutation permissions"),
    entry!(ProcessPrograms, "proc programs", [], "proc programs", "List bundled original Rust user executables.", "proc programs | json", ReadOnly, Records, "all domains"),
    entry!(ProcessList, "proc list", [], "proc list", "Read live and recent isolated user process states.", "proc list | json", ReadOnly, Records, "domain read policy"),
    entry!(ProcessRun, "proc run", [], "proc run PROGRAM [TEXT]", "Start a bundled program in ring 3 as user.", "proc run hello", Control, Number, "spawn permission; user task and frame quotas"),
    entry!(ProcessExec, "proc exec", [], "proc exec FILE [TEXT]", "Load a checked Tane executable readable by the user domain.", "proc exec hello.tane", Control, Number, "caller and user file-read policy; spawn and user quotas"),
    entry!(ProcessInstall, "proc install", [], "proc install PROGRAM FILE", "Save a bundled executable with the current domain's file label.", "proc install hello hello.tane", Write, Unit, "file ownership and current domain's storage quota"),
    entry!(ProcessWait, "proc wait", [], "proc wait PID", "Wait for one process, read its result and output; Ctrl-C cancels waiting.", "proc wait 2", Control, Unit, "domain read policy"),
    entry!(ProcessOutput, "proc output", [], "proc output PID", "Read bounded output captured from one user process.", "proc output 2", ReadOnly, Text, "domain read policy"),
    entry!(ProcessMemory, "proc memory", [], "proc memory PID", "Read a live process's mapped regions and permissions.", "proc memory 2 | json", ReadOnly, Records, "domain read policy"),
    entry!(ProcessPause, "proc pause", [], "proc pause PID", "Suspend a live user process while retaining its resources.", "proc pause 2", Control, Unit, "task control policy; user processes only"),
    entry!(ProcessResume, "proc resume", [], "proc resume PID", "Resume a suspended user process with its original wait state.", "proc resume 2", Control, Unit, "task control policy; user processes only"),
    entry!(StorageStatus, "file status", [], "file status", "Read filesystem format, recovery and durability mode.", "file status | json", ReadOnly, Records, "all domains"),
    entry!(StorageCheck, "file check", [], "file check", "Verify checksums of every file readable by this domain.", "file check | json", ReadOnly, Records, "file read policy"),
    entry!(StorageSync, "file sync", [], "file sync", "Flush the disk's write cache and report failure.", "file sync", Control, Unit, "admin only"),
    entry!(Rename, "file rename", [], "file rename OLD NEW", "Atomically rename one file without overwriting another.", "file rename notes archive", Write, Unit, "file write policy; label unchanged"),
    entry!(Truncate, "file truncate", [], "file truncate NAME BYTES", "Resize one file to 0-4096 bytes; growth is zero-filled.", "file truncate notes 12", Write, Unit, "file write policy"),
    entry!(Upgrade, "file upgrade", [], "file upgrade", "Upgrade a legacy volume without deleting files or labels.", "file upgrade", Write, Unit, "admin only"),
    entry!(Where, "where", [], "where FIELD OP VALUE", "Filter typed rows with == != < <= > >=.", "file list | where bytes > 0", ReadOnly, Records, "pipeline input only"),
    entry!(Select, "select", [], "select FIELD ...", "Project typed rows onto named columns.", "task list | select pid state", ReadOnly, Records, "pipeline input only"),
    entry!(Sort, "sort", [], "sort FIELD [--desc]", "Sort bounded typed rows by one column; ascending by default.", "file list | sort name", ReadOnly, Records, "pipeline input only"),
    entry!(Take, "take", [], "take COUNT", "Keep at most COUNT typed input rows.", "history | take 5", ReadOnly, Records, "pipeline input only"),
    entry!(Count, "count", [], "count", "Return the number of typed input rows.", "task list | count", ReadOnly, Number, "pipeline input only"),
    entry!(Json, "json", [], "json", "Render typed rows as escaped JSON.", "file list | json", ReadOnly, Text, "pipeline input only"),
    entry!(Save, "save", [], "save NAME", "Write the rendered pipeline result to one file.", "task list | json | save tasks", Write, Unit, "file ownership and current domain's storage quota"),
];

impl Operation {
    pub fn name(self) -> &'static str { metadata(self).name }

    pub const fn is_transform(self) -> bool {
        matches!(self, Self::Where | Self::Select | Self::Sort | Self::Take | Self::Count | Self::Json)
    }
}

pub fn metadata(operation: Operation) -> &'static Metadata { &OPERATIONS[operation as usize] }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub operation: Operation,
    /// Number of leading words belonging to the command name.
    pub argument_offset: usize,
}

/// Prefer canonical two-word commands before single-word legacy aliases.
/// In particular `net ping` cannot be consumed as the `net` status alias.
pub fn resolve(first: &str, second: Option<&str>) -> Option<Resolved> {
    for metadata in &OPERATIONS {
        if let Some((head, tail)) = metadata.name.split_once(' ') {
            if head == first && Some(tail) == second {
                return Some(Resolved { operation: metadata.operation, argument_offset: 2 });
            }
        }
    }
    for metadata in &OPERATIONS {
        if metadata.name == first || metadata.aliases.contains(&first) {
            return Some(Resolved { operation: metadata.operation, argument_offset: 1 });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_entry_is_resolvable_and_indexed_exactly_once() {
        for (index, record) in OPERATIONS.iter().enumerate() {
            assert_eq!(record.operation as usize, index);
            assert_eq!(metadata(record.operation).name, record.name);
            let (first, second) = match record.name.split_once(' ') {
                Some((first, second)) => (first, Some(second)), None => (record.name, None),
            };
            assert_eq!(resolve(first, second).unwrap().operation, record.operation);
            assert!(!record.usage.is_empty());
            assert!(!record.description.is_empty());
            assert!(!record.example.is_empty());
            assert!(!record.permission.is_empty());
            assert!(!record.schema.name().is_empty());
            // These fields populate the bounded `ops` record source.
            assert!(record.name.len() <= 64);
            assert!(record.effect.name().len() <= 64);
            assert!(record.schema.name().len() <= 64);
            assert!(record.permission.len() <= 64);
            for alias in record.aliases {
                assert_eq!(resolve(alias, None), Some(Resolved { operation: record.operation, argument_offset: 1 }));
            }
        }
    }

    #[test]
    fn legacy_aliases_reach_the_same_operations_without_shadowing_subcommands() {
        for (first, second, operation, offset) in [
            ("net", Some("ping"), Operation::Ping, 2),
            ("net", Some("status"), Operation::Network, 2),
            ("net", None, Operation::Network, 1),
            ("ping", Some("10.0.2.2"), Operation::Ping, 1),
            ("task", Some("list"), Operation::Tasks, 2),
            ("ps", None, Operation::Tasks, 1),
            ("file", Some("read"), Operation::Cat, 2),
            ("cat", Some("notes"), Operation::Cat, 1),
            ("file", Some("format"), Operation::Format, 2),
            ("format", None, Operation::Format, 1),
        ] {
            assert_eq!(resolve(first, second), Some(Resolved { operation, argument_offset: offset }));
        }
        for (first, second) in [("task", None), ("file", Some("execute")), ("Task", Some("list")), ("unknown", None)] {
            assert_eq!(resolve(first, second), None);
        }
    }

    #[test]
    fn catalog_names_and_aliases_do_not_collide() {
        for (index, record) in OPERATIONS.iter().enumerate() {
            for other in &OPERATIONS[index + 1..] {
                assert_ne!(record.name, other.name);
                for alias in record.aliases {
                    assert_ne!(*alias, other.name);
                    assert!(!other.aliases.contains(alias));
                }
                for alias in other.aliases { assert_ne!(*alias, record.name); }
            }
        }
    }

    #[test]
    fn effect_classification_does_not_hide_writes_or_network_transmission() {
        for operation in [Operation::Write, Operation::Append, Operation::Remove, Operation::Format, Operation::Save, Operation::Alloc, Operation::Free, Operation::Spawn, Operation::Kill, Operation::Let, Operation::Unset] {
            assert_eq!(metadata(operation).effect, Effect::Write);
        }
        for operation in [Operation::Halt, Operation::Reboot, Operation::Fault, Operation::DropToUser, Operation::Ping, Operation::Run, Operation::Apply] {
            assert_eq!(metadata(operation).effect, Effect::Control);
        }
        for operation in [Operation::Status, Operation::Tasks, Operation::List, Operation::Cat, Operation::Network, Operation::Ops, Operation::Where, Operation::Select, Operation::Sort, Operation::Take, Operation::Count, Operation::Json] {
            assert_eq!(metadata(operation).effect, Effect::ReadOnly);
        }
    }
}
