//! Allocation-free command parsing shared by the kernel and host tests.

#[derive(Debug, PartialEq, Eq)]
pub enum Action<'a> {
    Help,
    About,
    Memory,
    Clear,
    Halt,
    Reboot,
    Uptime,
    Alloc,
    Tasks,
    Security,
    Resources,
    Audit,
    DropToUser,
    Disk,
    Format,
    List,
    Cat(Result<&'a str, &'static str>),
    /// Name and text; `append` keeps the old contents.
    Write { name: &'a str, text: &'a str, append: bool },
    WriteUsage(&'static str),
    Remove(Result<&'a str, &'static str>),
    Echo(&'a str),
    Calc(Result<i64, &'static str>),
    Sleep(Result<u64, &'static str>),
    Fault(Result<Fault, &'static str>),
    Free(Result<u64, &'static str>),
    Spawn(Result<TaskKind, &'static str>),
    Kill(Result<u32, &'static str>),
    Empty,
    Unknown,
}

/// CPU exceptions that `fault` deliberately raises to exercise the IDT.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Fault {
    Breakpoint,
    DivideError,
    InvalidOpcode,
    GeneralProtection,
    PageFault,
    DoubleFault,
    /// Read the unmapped page at address 0.
    NullPointer,
    /// Write to the kernel's read-only code.
    ReadOnly,
    /// Jump into non-executable data.
    NoExecute,
}

/// Demonstration tasks that `spawn` can start.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TaskKind {
    /// CPU-bound loop; only the timer interrupt takes the CPU away.
    Spin,
    /// Sleeps 100 ms per beat, blocking instead of using the CPU.
    Beat,
    /// Sleeps 300 ms once, then exits so its stack frames are reclaimed.
    Once,
}

impl TaskKind {
    pub fn name(self) -> &'static str {
        match self {
            TaskKind::Spin => "spin",
            TaskKind::Beat => "beat",
            TaskKind::Once => "once",
        }
    }
}

pub const MAX_SLEEP_MS: u64 = 60_000;
const FAULT_USAGE: &str = "usage: fault bp|de|ud|gp|pf|df|null|ro|nx";
const SLEEP_USAGE: &str = "usage: sleep MS (0-60000)";
const FREE_USAGE: &str = "usage: free ADDR (0x hex or decimal)";
const SPAWN_USAGE: &str = "usage: spawn spin|beat|once";
const KILL_USAGE: &str = "usage: kill PID";

/// Trim the command line's outer ASCII whitespace. `echo` consumes one
/// separator after its name, preserving all remaining spaces inside the line.
pub fn parse(line: &str) -> Action<'_> {
    let line = line.trim_ascii();
    if line.is_empty() {
        return Action::Empty;
    }

    let separator = line.find(|c: char| c.is_ascii_whitespace());
    let (command, text) = match separator {
        Some(index) => (&line[..index], &line[index + 1..]),
        None => (line, ""),
    };

    match command {
        "echo" => Action::Echo(text),
        "calc" => Action::Calc(calculate(text)),
        "sleep" => Action::Sleep(sleep(text)),
        "fault" => Action::Fault(fault(text)),
        "free" => Action::Free(address(text)),
        "cat" => Action::Cat(one_name(text, "usage: cat NAME")),
        "rm" => Action::Remove(one_name(text, "usage: rm NAME")),
        "write" | "append" => {
            let append = command == "append";
            // One separator after the name; the rest of the line is kept as is.
            match text.split_once(|c: char| c.is_ascii_whitespace()) {
                Some((name, body)) if !name.is_empty() => Action::Write { name, text: body, append },
                _ if !text.is_empty() && !append => Action::Write { name: text, text: "", append },
                _ => Action::WriteUsage(if append { "usage: append NAME TEXT" } else { "usage: write NAME TEXT" }),
            }
        }
        "spawn" => Action::Spawn(spawn(text)),
        "kill" => Action::Kill(text.trim_ascii().parse::<u32>().ok().filter(|_| digits(text)).ok_or(KILL_USAGE)),
        _ if !text.is_empty() => Action::Unknown,
        "help" => Action::Help,
        "about" => Action::About,
        "mem" => Action::Memory,
        "uptime" => Action::Uptime,
        "alloc" => Action::Alloc,
        "ps" => Action::Tasks,
        "sec" => Action::Security,
        "top" => Action::Resources,
        "disk" => Action::Disk,
        "format" => Action::Format,
        "ls" => Action::List,
        "audit" => Action::Audit,
        "drop" => Action::DropToUser,
        "clear" => Action::Clear,
        "halt" => Action::Halt,
        "reboot" => Action::Reboot,
        _ => Action::Unknown,
    }
}

fn one_name<'a>(text: &'a str, usage: &'static str) -> Result<&'a str, &'static str> {
    let name = text.trim_ascii();
    if name.is_empty() || name.contains(|c: char| c.is_ascii_whitespace()) { Err(usage) } else { Ok(name) }
}

/// Rust's integer parsing accepts a leading `+`; commands take digits only.
fn digits(text: &str) -> bool {
    text.trim_ascii().bytes().all(|b| b.is_ascii_digit())
}

fn sleep(text: &str) -> Result<u64, &'static str> {
    match text.trim_ascii().parse::<u64>() {
        Ok(ms) if ms <= MAX_SLEEP_MS && digits(text) => Ok(ms),
        _ => Err(SLEEP_USAGE),
    }
}

fn address(text: &str) -> Result<u64, &'static str> {
    let text = text.trim_ascii();
    let parsed = match text.strip_prefix("0x") {
        Some(hex) if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => u64::from_str_radix(hex, 16),
        None if digits(text) => text.parse::<u64>(),
        _ => return Err(FREE_USAGE),
    };
    parsed.map_err(|_| FREE_USAGE)
}

fn spawn(text: &str) -> Result<TaskKind, &'static str> {
    match text.trim_ascii() {
        "spin" => Ok(TaskKind::Spin),
        "beat" => Ok(TaskKind::Beat),
        "once" => Ok(TaskKind::Once),
        _ => Err(SPAWN_USAGE),
    }
}

fn fault(text: &str) -> Result<Fault, &'static str> {
    match text.trim_ascii() {
        "bp" => Ok(Fault::Breakpoint),
        "de" => Ok(Fault::DivideError),
        "ud" => Ok(Fault::InvalidOpcode),
        "gp" => Ok(Fault::GeneralProtection),
        "pf" => Ok(Fault::PageFault),
        "df" => Ok(Fault::DoubleFault),
        "null" => Ok(Fault::NullPointer),
        "ro" => Ok(Fault::ReadOnly),
        "nx" => Ok(Fault::NoExecute),
        _ => Err(FAULT_USAGE),
    }
}

fn calculate(text: &str) -> Result<i64, &'static str> {
    let mut words = text.split_ascii_whitespace();
    let left = words.next().ok_or("usage: calc I64 (+|-|*|/) I64")?;
    let operator = words.next().ok_or("usage: calc I64 (+|-|*|/) I64")?;
    let right = words.next().ok_or("usage: calc I64 (+|-|*|/) I64")?;
    if words.next().is_some() {
        return Err("usage: calc I64 (+|-|*|/) I64");
    }

    let left = left.parse::<i64>().map_err(|_| "invalid i64 integer")?;
    let right = right.parse::<i64>().map_err(|_| "invalid i64 integer")?;
    let value = match operator {
        "+" => left.checked_add(right),
        "-" => left.checked_sub(right),
        "*" => left.checked_mul(right),
        "/" if right == 0 => return Err("division by zero"),
        "/" => left.checked_div(right),
        _ => return Err("operator must be +, -, *, or /"),
    };
    value.ok_or("i64 overflow")
}

#[cfg(test)]
mod tests {
    use super::{parse, Action, Fault, TaskKind};

    #[test]
    fn exact_commands_and_outer_whitespace() {
        for (line, expected) in [
            ("help", Action::Help),
            ("\t about \r\n", Action::About),
            ("mem", Action::Memory),
            ("clear", Action::Clear),
            ("halt", Action::Halt),
            ("reboot", Action::Reboot),
            ("uptime", Action::Uptime),
            ("alloc", Action::Alloc),
            ("ps", Action::Tasks),
            ("sec", Action::Security),
            ("audit", Action::Audit),
            ("drop", Action::DropToUser),
            ("top", Action::Resources),
            ("disk", Action::Disk),
            ("format", Action::Format),
            ("ls", Action::List),
        ] {
            assert_eq!(parse(line), expected);
        }
    }

    #[test]
    fn empty_and_unknown_input() {
        assert_eq!(parse(""), Action::Empty);
        assert_eq!(parse(" \t\r\n"), Action::Empty);
        assert_eq!(parse("HELP"), Action::Unknown);
        assert_eq!(parse("mem extra"), Action::Unknown);
        assert_eq!(parse("uptime now"), Action::Unknown);
        assert_eq!(parse("alloc 2"), Action::Unknown);
        assert_eq!(parse("ps -a"), Action::Unknown);
        assert_eq!(parse("drop admin"), Action::Unknown);
        assert_eq!(parse("su"), Action::Unknown);
        assert_eq!(parse("echoes"), Action::Unknown);
        assert_eq!(parse("\u{2003}"), Action::Unknown);
    }

    #[test]
    fn echo_borrows_and_preserves_content() {
        assert_eq!(parse("echo"), Action::Echo(""));
        assert_eq!(parse(" echo Hello, Rust!  "), Action::Echo("Hello, Rust!"));
        assert_eq!(parse("echo  a   b"), Action::Echo(" a   b"));
        assert_eq!(parse("echo\ta\tb"), Action::Echo("a\tb"));
        assert_eq!(parse("echo \u{03bb}"), Action::Echo("\u{03bb}"));
    }

    #[test]
    fn signed_arithmetic_and_integer_division() {
        for (line, expected) in [
            ("calc 6 + 7", 13),
            ("calc\t+6\t-\t-7", 13),
            ("calc -6 * 7", -42),
            ("calc -7 / 3", -2),
            ("calc -9223372036854775808 + 0", i64::MIN),
        ] {
            assert_eq!(parse(line), Action::Calc(Ok(expected)));
        }
    }

    #[test]
    fn checked_arithmetic_rejects_overflow_and_zero_division() {
        for line in [
            "calc 9223372036854775807 + 1",
            "calc -9223372036854775808 - 1",
            "calc 9223372036854775807 * 2",
            "calc -9223372036854775808 / -1",
        ] {
            assert_eq!(parse(line), Action::Calc(Err("i64 overflow")));
        }
        assert_eq!(parse("calc 5 / 0"), Action::Calc(Err("division by zero")));
    }

    #[test]
    fn malformed_expressions_are_rejected() {
        for line in ["calc", "calc 1", "calc 1 +", "calc 1+2", "calc 1 + 2 x"] {
            assert_eq!(parse(line), Action::Calc(Err("usage: calc I64 (+|-|*|/) I64")));
        }
        for line in ["calc x + 2", "calc 9223372036854775808 + 0"] {
            assert_eq!(parse(line), Action::Calc(Err("invalid i64 integer")));
        }
        assert_eq!(parse("calc 1 % 2"), Action::Calc(Err("operator must be +, -, *, or /")));
    }

    #[test]
    fn sleep_accepts_bounded_milliseconds() {
        assert_eq!(parse("sleep 0"), Action::Sleep(Ok(0)));
        assert_eq!(parse("sleep  250 "), Action::Sleep(Ok(250)));
        assert_eq!(parse("sleep 60000"), Action::Sleep(Ok(60_000)));
        for line in ["sleep", "sleep 60001", "sleep -1", "sleep +5", "sleep 1 2", "sleep x"] {
            assert_eq!(parse(line), Action::Sleep(Err("usage: sleep MS (0-60000)")));
        }
    }

    #[test]
    fn fault_names_select_one_exception() {
        for (line, expected) in [
            ("fault bp", Fault::Breakpoint),
            ("fault de", Fault::DivideError),
            ("fault ud", Fault::InvalidOpcode),
            ("fault gp", Fault::GeneralProtection),
            ("fault pf", Fault::PageFault),
            ("fault  df ", Fault::DoubleFault),
            ("fault null", Fault::NullPointer),
            ("fault ro", Fault::ReadOnly),
            ("fault nx", Fault::NoExecute),
        ] {
            assert_eq!(parse(line), Action::Fault(Ok(expected)));
        }
        for line in ["fault", "fault PF", "fault pf gp", "fault nmi"] {
            assert_eq!(parse(line), Action::Fault(Err("usage: fault bp|de|ud|gp|pf|df|null|ro|nx")));
        }
    }

    #[test]
    fn free_takes_one_hex_or_decimal_address() {
        assert_eq!(parse("free 0x100000"), Action::Free(Ok(0x100000)));
        assert_eq!(parse("free 0xABCdef"), Action::Free(Ok(0xabcdef)));
        assert_eq!(parse("free 4096"), Action::Free(Ok(4096)));
        assert_eq!(parse("free 0xffffffffffffffff"), Action::Free(Ok(u64::MAX)));
        for line in ["free", "free 0x", "free 0x1g", "free +4096", "free -1", "free 1 2",
                     "free 0x10000000000000000", "free 18446744073709551616", "free 0X10"] {
            assert_eq!(parse(line), Action::Free(Err("usage: free ADDR (0x hex or decimal)")), "{line}");
        }
    }

    #[test]
    fn spawn_and_kill_arguments() {
        assert_eq!(parse("spawn spin"), Action::Spawn(Ok(TaskKind::Spin)));
        assert_eq!(parse("spawn beat"), Action::Spawn(Ok(TaskKind::Beat)));
        assert_eq!(parse("spawn  once "), Action::Spawn(Ok(TaskKind::Once)));
        for line in ["spawn", "spawn idle", "spawn spin beat"] {
            assert_eq!(parse(line), Action::Spawn(Err("usage: spawn spin|beat|once")));
        }
        assert_eq!(parse("kill 7"), Action::Kill(Ok(7)));
        for line in ["kill", "kill +7", "kill -1", "kill x", "kill 4294967296", "kill 1 2"] {
            assert_eq!(parse(line), Action::Kill(Err("usage: kill PID")), "{line}");
        }
        assert_eq!(TaskKind::Beat.name(), "beat");
    }

    #[test]
    fn file_commands() {
        assert_eq!(parse("cat notes"), Action::Cat(Ok("notes")));
        assert_eq!(parse("cat"), Action::Cat(Err("usage: cat NAME")));
        assert_eq!(parse("cat a b"), Action::Cat(Err("usage: cat NAME")));
        assert_eq!(parse("rm  old "), Action::Remove(Ok("old")));
        assert_eq!(parse("rm"), Action::Remove(Err("usage: rm NAME")));
        assert_eq!(parse("write notes hello  world"), Action::Write { name: "notes", text: "hello  world", append: false });
        assert_eq!(parse("write empty"), Action::Write { name: "empty", text: "", append: false });
        assert_eq!(parse("append log  line"), Action::Write { name: "log", text: " line", append: true });
        assert_eq!(parse("write"), Action::WriteUsage("usage: write NAME TEXT"));
        assert_eq!(parse("append log"), Action::WriteUsage("usage: append NAME TEXT"));
        assert_eq!(parse("ls -l"), Action::Unknown);
        assert_eq!(parse("format now"), Action::Unknown);
    }
}
