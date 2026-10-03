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
    Echo(&'a str),
    Calc(Result<i64, &'static str>),
    Sleep(Result<u64, &'static str>),
    Fault(Result<Fault, &'static str>),
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
}

pub const MAX_SLEEP_MS: u64 = 60_000;
const FAULT_USAGE: &str = "usage: fault bp|de|ud|gp|pf|df";
const SLEEP_USAGE: &str = "usage: sleep MS (0-60000)";

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
        _ if !text.is_empty() => Action::Unknown,
        "help" => Action::Help,
        "about" => Action::About,
        "mem" => Action::Memory,
        "uptime" => Action::Uptime,
        "clear" => Action::Clear,
        "halt" => Action::Halt,
        "reboot" => Action::Reboot,
        _ => Action::Unknown,
    }
}

fn sleep(text: &str) -> Result<u64, &'static str> {
    match text.trim_ascii().parse::<u64>() {
        Ok(ms) if ms <= MAX_SLEEP_MS && text.trim_ascii().bytes().all(|b| b.is_ascii_digit()) => Ok(ms),
        _ => Err(SLEEP_USAGE),
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
    use super::{parse, Action, Fault};

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
        ] {
            assert_eq!(parse(line), Action::Fault(Ok(expected)));
        }
        for line in ["fault", "fault PF", "fault pf gp", "fault nmi"] {
            assert_eq!(parse(line), Action::Fault(Err("usage: fault bp|de|ud|gp|pf|df")));
        }
    }
}
