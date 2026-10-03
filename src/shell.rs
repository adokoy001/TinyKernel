//! Allocation-free command parsing shared by the kernel and host tests.

#[derive(Debug, PartialEq, Eq)]
pub enum Action<'a> {
    Help,
    About,
    Memory,
    Clear,
    Halt,
    Reboot,
    Echo(&'a str),
    Calc(Result<i64, &'static str>),
    Empty,
    Unknown,
}

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
        _ if !text.is_empty() => Action::Unknown,
        "help" => Action::Help,
        "about" => Action::About,
        "mem" => Action::Memory,
        "clear" => Action::Clear,
        "halt" => Action::Halt,
        "reboot" => Action::Reboot,
        _ => Action::Unknown,
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
    use super::{parse, Action};

    #[test]
    fn exact_commands_and_outer_whitespace() {
        for (line, expected) in [
            ("help", Action::Help),
            ("\t about \r\n", Action::About),
            ("mem", Action::Memory),
            ("clear", Action::Clear),
            ("halt", Action::Halt),
            ("reboot", Action::Reboot),
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
}
