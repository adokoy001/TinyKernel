//! Embedded executables are built by users/build.py before the kernel.
pub struct Program { pub name: &'static str, pub bytes: &'static [u8] }
pub static PROGRAMS: &[Program] = &[
    Program { name: "hello", bytes: include_bytes!("../build/users/hello.tane") },
    Program { name: "echo", bytes: include_bytes!("../build/users/echo.tane") },
    Program { name: "busy", bytes: include_bytes!("../build/users/busy.tane") },
    Program { name: "sleep", bytes: include_bytes!("../build/users/sleep.tane") },
    Program { name: "isolate", bytes: include_bytes!("../build/users/isolate.tane") },
    Program { name: "files", bytes: include_bytes!("../build/users/files.tane") },
    Program { name: "probe", bytes: include_bytes!("../build/users/probe.tane") },
];
pub fn builtin(name: &str) -> Option<&'static [u8]> { PROGRAMS.iter().find(|program| program.name == name).map(|program| program.bytes) }
