//! Shell integration. Syntax and every pipeline contract are checked before
//! a source runs. Expansion is literal data; decoded words are never reparsed.

use core::fmt::{self, Write};
use core::mem::MaybeUninit;
use core::ptr::{addr_of, addr_of_mut};
use crate::{editor::Editor, fs, inet, interrupts, mac::{self, Domain, Op}, net,
    operations::{self, Operation as O, Resolved}, plans::{Plan, PlannedKind},
    records::{self, Cell, Column, Kind, Table, Transform}, resources, security,
    shell::{self, Action, Fault, PingRequest, TaskKind}, shell_lang::{self, Pipeline},
    storage, tasks, variables::Variables, ExecState, Keyboard};

// The shell is the sole owner. Interrupt handlers never access these buffers.
static mut TABLE: Table = Table::new();
static mut VARIABLES: Variables = Variables::new();
static mut PLAN: Plan = Plan::new();
static mut LAST: Last = Last { state: ExecState::Success, operation: "boot", message: "ready" };
static mut SCRIPT_DEPTH: usize = 0;
static mut SCRIPT_BUDGET: usize = 128;
static mut PING_OUTCOME: ExecState = ExecState::Success;

#[derive(Clone, Copy)]
struct Last { state: ExecState, operation: &'static str, message: &'static str }

fn table() -> &'static mut Table { unsafe { &mut *addr_of_mut!(TABLE) } }
fn variables() -> &'static mut Variables { unsafe { &mut *addr_of_mut!(VARIABLES) } }
fn variable_view() -> &'static Variables { unsafe { &*addr_of!(VARIABLES) } }
fn plan() -> &'static mut Plan { unsafe { &mut *addr_of_mut!(PLAN) } }
fn last() -> Last { unsafe { *addr_of_mut!(LAST) } }

fn code(state: ExecState) -> &'static str {
    match state { ExecState::Success => "success", ExecState::Error => "error",
        ExecState::Denied => "denied", ExecState::Cancelled => "cancelled",
        ExecState::CommitUnknown => "commit-unknown" }
}

fn remember(state: ExecState, operation: &'static str, message: &'static str) {
    unsafe { *addr_of_mut!(LAST) = Last { state, operation, message }; }
}

fn finish(state: ExecState, operation: O) -> ExecState {
    let message = match state { ExecState::Success => "completed", ExecState::Error => "command failed",
        ExecState::Denied => "permission denied", ExecState::Cancelled => "cancelled",
        ExecState::CommitUnknown => "disk write failed; commit state unknown" };
    remember(state, operation.name(), message);
    state
}

pub fn reject_input() { remember(ExecState::Error, "input", "input line exceeds 255 bytes"); }
pub fn cancel_input() { remember(ExecState::Cancelled, "input", "cancelled"); }

fn lookup(name: &str) -> Option<&'static str> {
    if name == "STATUS" { Some(code(last().state)) } else { variable_view().lookup(name) }
}

/// The caller owns an independent copy of the editor line during execution.
pub fn execute(line: &str, keyboard: &mut Keyboard, editor: &mut Editor) {
    unsafe { *addr_of_mut!(SCRIPT_BUDGET) = 128; }
    execute_line(line, keyboard, editor);
}

#[derive(Clone, Copy)]
struct Prepared {
    source: Resolved,
    transforms: [Option<Transform>; shell_lang::MAX_STAGES],
    transform_len: usize,
    json: bool,
    save: Option<usize>,
}

impl Prepared {
    const EMPTY: Self = Self { source: Resolved { operation: O::Help, argument_offset: 0 },
        transforms: [None; shell_lang::MAX_STAGES], transform_len: 0, json: false, save: None };
}

// The top-level command and each of four nested script levels own a separate
// slot. Keeping validated transforms here avoids multi-kilobyte Result copies
// at every recursive call. MaybeUninit keeps the storage entirely in BSS.
static mut PREPARED: [MaybeUninit<Prepared>; 5] = [const { MaybeUninit::uninit() }; 5];

fn resolve(pipeline: &Pipeline, stage: usize) -> Result<Resolved, &'static str> {
    if pipeline.word_expanded(stage, 0) { return Err("operation names must be literal"); }
    let resolved = operations::resolve(word(pipeline, stage, 0), pipeline.word(stage, 1))
        .ok_or("unknown operation; type help")?;
    if resolved.argument_offset == 2 && pipeline.word_expanded(stage, 1) {
        return Err("subcommand names must be literal");
    }
    Ok(resolved)
}

fn word(pipeline: &Pipeline, stage: usize, index: usize) -> &str {
    pipeline.word(stage, index).unwrap_or("")
}

fn args<'a>(pipeline: &'a Pipeline, stage: usize, offset: usize,
    output: &mut [&'a str; shell_lang::MAX_WORDS]) -> usize {
    let count = pipeline.word_count(stage).saturating_sub(offset);
    for index in 0..count { output[index] = word(pipeline, stage, index + offset); }
    count
}

fn source_allowed(operation: O) -> bool {
    matches!(operation, O::About | O::Status | O::Memory | O::Uptime | O::Tasks | O::Security |
        O::Resources | O::Audit | O::Disk | O::Network | O::Ping | O::List | O::Cat |
        O::Echo | O::Calc | O::Ops | O::Vars | O::History)
}

const TEXT_SCHEMA: &[Column] = &[Column::new("text", Kind::Text)];
const NUMBER_SCHEMA: &[Column] = &[Column::new("value", Kind::Int)];
const UPTIME_SCHEMA: &[Column] = &[Column::new("milliseconds", Kind::UInt), Column::new("ticks", Kind::UInt)];
const STATUS_SCHEMA: &[Column] = &[Column::new("code", Kind::Text), Column::new("operation", Kind::Text), Column::new("message", Kind::Text)];
const MEMORY_SCHEMA: &[Column] = &[Column::new("usable", Kind::UInt), Column::new("free", Kind::UInt), Column::new("used", Kind::UInt), Column::new("frame_bytes", Kind::UInt)];
const TASK_SCHEMA: &[Column] = &[Column::new("pid", Kind::UInt), Column::new("name", Kind::Text), Column::new("domain", Kind::Text), Column::new("state", Kind::Text), Column::new("cpu_ticks", Kind::UInt), Column::new("counter", Kind::UInt), Column::new("stack", Kind::UInt)];
const FILE_SCHEMA: &[Column] = &[Column::new("slot", Kind::UInt), Column::new("label", Kind::Text), Column::new("bytes", Kind::UInt), Column::new("generation", Kind::UInt), Column::new("name", Kind::Text)];
const NETWORK_SCHEMA: &[Column] = &[Column::new("available", Kind::Bool), Column::new("mac", Kind::Text), Column::new("ipv4", Kind::Text), Column::new("ipv6", Kind::Text), Column::new("rx", Kind::UInt), Column::new("tx", Kind::UInt), Column::new("dropped", Kind::UInt), Column::new("neighbors", Kind::UInt)];
const PING_SCHEMA: &[Column] = &[Column::new("kind", Kind::Text), Column::new("source", Kind::Text), Column::new("sequence", Kind::UInt), Column::new("bytes", Kind::UInt), Column::new("ttl", Kind::UInt), Column::new("rtt_ms", Kind::UInt), Column::new("sent", Kind::UInt), Column::new("received", Kind::UInt)];
const SECURITY_SCHEMA: &[Column] = &[Column::new("domain", Kind::Text), Column::new("denials", Kind::UInt), Column::new("policy_rules", Kind::UInt)];
const RESOURCE_SCHEMA: &[Column] = &[Column::new("domain", Kind::Text), Column::new("tasks", Kind::UInt), Column::new("frames", Kind::UInt), Column::new("files", Kind::UInt), Column::new("cpu_ticks", Kind::UInt), Column::new("cpu_percent", Kind::UInt), Column::new("task_limit", Kind::UInt), Column::new("frame_limit", Kind::UInt)];
const AUDIT_SCHEMA: &[Column] = &[Column::new("id", Kind::UInt), Column::new("ticks", Kind::UInt), Column::new("pid", Kind::UInt), Column::new("subject", Kind::Text), Column::new("operation", Kind::Text), Column::new("object", Kind::Text), Column::new("target", Kind::UInt), Column::new("reason", Kind::Text)];
const DISK_SCHEMA: &[Column] = &[Column::new("available", Kind::Bool), Column::new("mounted", Kind::Bool), Column::new("model", Kind::Text), Column::new("sectors", Kind::UInt), Column::new("files", Kind::UInt), Column::new("error", Kind::Text)];
const OPS_SCHEMA: &[Column] = &[Column::new("name", Kind::Text), Column::new("effect", Kind::Text), Column::new("result", Kind::Text), Column::new("permission", Kind::Text)];
const VAR_SCHEMA: &[Column] = &[Column::new("name", Kind::Text), Column::new("value", Kind::Text)];
const HISTORY_SCHEMA: &[Column] = &[Column::new("id", Kind::UInt), Column::new("part", Kind::UInt), Column::new("command", Kind::Text)];

fn schema(operation: O) -> &'static [Column] {
    match operation {
        O::About | O::Cat | O::Echo => TEXT_SCHEMA, O::Calc => NUMBER_SCHEMA,
        O::Uptime => UPTIME_SCHEMA, O::Status => STATUS_SCHEMA, O::Memory => MEMORY_SCHEMA,
        O::Tasks => TASK_SCHEMA, O::List => FILE_SCHEMA, O::Network => NETWORK_SCHEMA,
        O::Ping => PING_SCHEMA, O::Security => SECURITY_SCHEMA, O::Resources => RESOURCE_SCHEMA,
        O::Audit => AUDIT_SCHEMA, O::Disk => DISK_SCHEMA, O::Ops => OPS_SCHEMA,
        O::Vars => VAR_SCHEMA, O::History => HISTORY_SCHEMA, _ => &[],
    }
}

#[inline(never)]
fn prepare(pipeline: &Pipeline, result: &mut Prepared) -> Result<(), &'static str> {
    let source = resolve(pipeline, 0)?;
    let mut words = [""; shell_lang::MAX_WORDS];
    let count = args(pipeline, 0, source.argument_offset, &mut words);
    validate(source.operation, &words[..count])?;
    if source.operation.is_transform() || source.operation == O::Save {
        return Err("this operation requires pipeline input");
    }
    if source.operation == O::Plan && count > 0 {
        if pipeline.word_expanded(0, source.argument_offset) {
            return Err("planned operation names must be literal");
        }
        if let Some(target) = operations::resolve(words[0], words.get(1).copied().filter(|_| count > 1)) {
            if target.argument_offset == 2 && pipeline.word_expanded(0, source.argument_offset + 1) {
                return Err("planned subcommand names must be literal");
            }
        }
    }
    result.source = source;
    result.transforms.fill(None);
    result.transform_len = 0;
    result.json = false;
    result.save = None;
    if pipeline.stage_count() == 1 { return Ok(()); }
    if !source_allowed(source.operation) { return Err("pipeline sources must be read operations or bounded ping"); }
    let mut current = records::Schema::new(schema(source.operation)).map_err(|error| error.description())?;
    for stage in 1..pipeline.stage_count() {
        let resolved = resolve(pipeline, stage)?;
        let count = args(pipeline, stage, resolved.argument_offset, &mut words);
        match resolved.operation {
            O::Save => {
                validate(O::Save, &words[..count])?;
                if stage + 1 != pipeline.stage_count() { return Err("save must be the final pipeline stage"); }
                result.save = Some(stage);
            }
            O::Json => {
                if count != 0 || result.json { return Err("json takes no arguments and occurs once"); }
                if stage + 1 < pipeline.stage_count() {
                    let next = resolve(pipeline, stage + 1)?;
                    if next.operation != O::Save { return Err("json must be final or followed only by save"); }
                }
                result.json = true;
            }
            O::Where | O::Select | O::Sort | O::Take | O::Count => {
                if result.json { return Err("record transforms must precede json"); }
                let count = args(pipeline, stage, 0, &mut words);
                let transform = Transform::parse(current.columns(), &words[..count])
                    .map_err(|error| error.description())?;
                current = records::Schema::new(transform.output_schema()).map_err(|error| error.description())?;
                result.transforms[result.transform_len] = Some(transform);
                result.transform_len += 1;
            }
            _ => return Err("pipeline stage must be where, select, sort, take, count, json or save"),
        }
    }
    Ok(())
}

fn exact(arguments: &[&str], count: usize, usage: &'static str) -> Result<(), &'static str> {
    if arguments.len() == count { Ok(()) } else { Err(usage) }
}

fn file_name(name: &str) -> Result<(), &'static str> {
    if fs::valid_name(name.as_bytes()) { Ok(()) } else { Err("invalid TaneFS name") }
}

fn unsigned(text: &str) -> Result<u64, &'static str> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) { return Err("expected unsigned decimal integer"); }
    text.parse().map_err(|_| "integer out of range")
}

fn address(text: &str) -> Result<u64, &'static str> {
    if let Some(hex) = text.strip_prefix("0x") {
        if hex.is_empty() || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) { return Err("invalid hexadecimal address"); }
        u64::from_str_radix(hex, 16).map_err(|_| "address out of range")
    } else { unsigned(text) }
}

fn calculate(arguments: &[&str]) -> Result<i64, &'static str> {
    exact(arguments, 3, "usage: calc I64 (+|-|*|/) I64")?;
    let left = arguments[0].parse::<i64>().map_err(|_| "invalid i64 integer")?;
    let right = arguments[2].parse::<i64>().map_err(|_| "invalid i64 integer")?;
    let value = match arguments[1] { "+" => left.checked_add(right), "-" => left.checked_sub(right),
        "*" => left.checked_mul(right), "/" if right == 0 => return Err("division by zero"),
        "/" => left.checked_div(right), _ => return Err("operator must be +, -, *, or /") };
    value.ok_or("i64 overflow")
}

fn task_kind(value: &str) -> Result<TaskKind, &'static str> {
    match value { "spin" => Ok(TaskKind::Spin), "beat" => Ok(TaskKind::Beat),
        "once" => Ok(TaskKind::Once), _ => Err("usage: spawn spin|beat|once") }
}

fn fault(value: &str) -> Result<Fault, &'static str> {
    match value { "bp" => Ok(Fault::Breakpoint), "de" => Ok(Fault::DivideError),
        "ud" => Ok(Fault::InvalidOpcode), "gp" => Ok(Fault::GeneralProtection),
        "pf" => Ok(Fault::PageFault), "df" => Ok(Fault::DoubleFault),
        "null" => Ok(Fault::NullPointer), "ro" => Ok(Fault::ReadOnly),
        "nx" => Ok(Fault::NoExecute), _ => Err("usage: fault bp|de|ud|gp|pf|df|null|ro|nx") }
}

fn ping_request<'a>(arguments: &[&'a str]) -> Result<PingRequest<'a>, &'static str> {
    const USAGE: &str = "usage: net ping IP [--count 1-10] [--timeout 1-5000] (milliseconds)";
    let address = arguments.first().copied().ok_or(USAGE)?;
    if inet::parse(address).is_none() { return Err("invalid literal IPv4/IPv6 address (DNS is not implemented)"); }
    let mut request = PingRequest { address, count: 3, timeout_ms: 1000 };
    let (mut count_seen, mut timeout_seen) = (false, false);
    let mut index = 1;
    while index < arguments.len() {
        let value = unsigned(arguments.get(index + 1).copied().ok_or(USAGE)?).map_err(|_| USAGE)?;
        match arguments[index] {
            "--count" if !count_seen && (1..=10).contains(&value) => { request.count = value as u8; count_seen = true; }
            "--timeout" if !timeout_seen && (1..=5000).contains(&value) => { request.timeout_ms = value; timeout_seen = true; }
            _ => return Err(USAGE),
        }
        index += 2;
    }
    Ok(request)
}

fn validate(operation: O, arguments: &[&str]) -> Result<(), &'static str> {
    let usage = operations::metadata(operation).usage;
    match operation {
        O::Help => {
            if arguments.len() > 2 { return Err(usage); }
            if !arguments.is_empty() {
                let resolved = operations::resolve(arguments[0], arguments.get(1).copied()).ok_or("unknown operation")?;
                if resolved.argument_offset != arguments.len() { return Err(usage); }
            }
            Ok(())
        }
        O::Echo => Ok(()),
        O::Calc => calculate(arguments).map(|_| ()),
        O::Ping => ping_request(arguments).map(|_| ()),
        O::Write | O::Append => {
            if arguments.is_empty() || (operation == O::Append && arguments.len() < 2) { return Err(usage); }
            file_name(arguments[0])
        }
        O::Cat | O::Remove | O::Run | O::Save => { exact(arguments, 1, usage)?; file_name(arguments[0]) }
        O::Sleep => {
            exact(arguments, 1, "usage: sleep MS (0-60000)")?;
            if unsigned(arguments[0]).map_err(|_| "usage: sleep MS (0-60000)")? > shell::MAX_SLEEP_MS {
                return Err("usage: sleep MS (0-60000)");
            }
            Ok(())
        }
        O::Free => { exact(arguments, 1, "usage: free ADDR (0x hex or decimal)")?; address(arguments[0]).map(|_| ()) }
        O::Spawn => { exact(arguments, 1, "usage: spawn spin|beat|once")?; task_kind(arguments[0]).map(|_| ()) }
        O::Kill => {
            exact(arguments, 1, "usage: kill PID")?;
            if unsigned(arguments[0]).map_err(|_| "usage: kill PID")? > u32::MAX as u64 { return Err("usage: kill PID"); }
            Ok(())
        }
        O::Fault => { exact(arguments, 1, "usage: fault bp|de|ud|gp|pf|df|null|ro|nx")?; fault(arguments[0]).map(|_| ()) }
        O::Let => {
            exact(arguments, 2, usage)?;
            if !crate::variables::valid_name(arguments[0]) { return Err("invalid variable name"); }
            if arguments[0] == "STATUS" { return Err("STATUS is read-only"); }
            if arguments[1].len() > crate::variables::MAX_VALUE { return Err("variable value exceeds 64 bytes"); }
            Ok(())
        }
        O::Unset => {
            exact(arguments, 1, usage)?;
            if !crate::variables::valid_name(arguments[0]) { return Err("invalid variable name"); }
            if arguments[0] == "STATUS" { return Err("STATUS is read-only"); }
            Ok(())
        }
        O::Apply => {
            if arguments.len() > 1 { return Err(usage); }
            if let Some(id) = arguments.first() { unsigned(id)?; }
            Ok(())
        }
        O::Plan => {
            if arguments.is_empty() { return Ok(()); }
            let target = operations::resolve(arguments[0], arguments.get(1).copied()).ok_or("plan requires file write, append or remove")?;
            if !matches!(target.operation, O::Write | O::Append | O::Remove) { return Err("plan requires file write, append or remove"); }
            validate(target.operation, &arguments[target.argument_offset..])
        }
        O::Where | O::Select | O::Sort | O::Take | O::Count | O::Json => Err("this operation requires pipeline input"),
        _ => exact(arguments, 0, usage),
    }
}

fn error(message: &'static str) -> ExecState { kprintln!("error: {}", message); ExecState::Error }

fn record_error(error: records::Error) -> ExecState { error_message(error.description()) }
fn error_message(message: &'static str) -> ExecState { error(message) }

fn storage_error(failure: storage::StorageError, mutating: bool) -> ExecState {
    let state = if mutating && failure.commit_unknown() { ExecState::CommitUnknown }
        else if matches!(failure, storage::StorageError::Denied(_)) { ExecState::Denied }
        else { ExecState::Error };
    crate::report_storage(failure);
    if matches!(state, ExecState::CommitUnknown) { kprintln!("commit-unknown: disk sectors may already have changed"); }
    state
}

#[inline(never)]
fn execute_line(line: &str, keyboard: &mut Keyboard, editor: &mut Editor) -> ExecState {
    let pipeline = match shell_lang::parse(line, lookup) {
        Ok(pipeline) => pipeline,
        Err(failure) => {
            kprintln!("error: {} at byte {}", failure.message(), failure.offset);
            remember(ExecState::Error, "syntax", failure.message());
            return ExecState::Error;
        }
    };
    if pipeline.stage_count() == 0 { return ExecState::Success; }
    let depth = unsafe { *addr_of_mut!(SCRIPT_DEPTH) };
    if depth >= 5 { return error("script execution context is out of range"); }
    // Borrow only this raw-pointer-selected slot, not the entire array: outer
    // recursive invocations keep their distinct slots alive until returning.
    let prepared = unsafe { (&mut *addr_of_mut!(PREPARED).cast::<MaybeUninit<Prepared>>().add(depth)).write(Prepared::EMPTY) };
    if let Err(message) = prepare(&pipeline, prepared) {
            let operation = resolve(&pipeline, 0).ok().map(|resolved| resolved.operation.name()).unwrap_or("unknown");
            error(message);
            remember(ExecState::Error, operation, message);
            return ExecState::Error;
    }
    let operation = prepared.source.operation;
    let mut words = [""; shell_lang::MAX_WORDS];
    let count = args(&pipeline, 0, prepared.source.argument_offset, &mut words);
    let arguments = &words[..count];
    if pipeline.stage_count() == 1 {
        let state = single(operation, arguments, line, prepared.source, keyboard, editor);
        if operation == O::Status { return state; }
        return finish(state, operation);
    }
    // A sink's permission, disk and slot/quota availability are knowable
    // before a source runs, including a source which transmits packets.
    // The exact rendered byte length is checked again before the write.
    if let Some(stage) = prepared.save {
        if let Err(failure) = storage::preflight(word(&pipeline, stage, 1), PlannedKind::Write, 0) {
            return finish(storage_error(failure, false), O::Save);
        }
    }
    let mut state = source(operation, arguments, keyboard, editor);
    if !matches!(state, ExecState::Success) { return finish(state, operation); }
    let source_outcome = if operation == O::Ping { unsafe { *addr_of_mut!(PING_OUTCOME) } }
        else { ExecState::Success };
    for transform in prepared.transforms[..prepared.transform_len].iter().flatten() {
        if let Err(failure) = transform.apply(table()) { return finish(record_error(failure), operation); }
    }
    if let Some(stage) = prepared.save {
        state = save_result(word(&pipeline, stage, 1), operation, prepared.json, prepared.transform_len == 0);
        if matches!(state, ExecState::Success) { state = source_outcome; }
        return finish(state, O::Save);
    }
    state = source_outcome;
    let result = if prepared.json { records::write_json(table(), crate::console()) }
        else { records::write_table(table(), crate::console()) };
    if let Err(failure) = result { state = record_error(failure); }
    if operation == O::Status { state } else { finish(state, operation) }
}

/// Small bounded UTF-8 buffer; writes either fit entirely or fail.
struct Buffer<const N: usize> { bytes: [u8; N], len: usize }
impl<const N: usize> Buffer<N> {
    const fn new() -> Self { Self { bytes: [0; N], len: 0 } }
    fn text(&self) -> &str { core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("") }
}
impl<const N: usize> Write for Buffer<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if text.len() > N - self.len { return Err(fmt::Error); }
        self.bytes[self.len..self.len + text.len()].copy_from_slice(text.as_bytes());
        self.len += text.len();
        Ok(())
    }
}

fn joined<'a>(arguments: &[&str], buffer: &'a mut Buffer<512>) -> Result<&'a str, &'static str> {
    for (index, argument) in arguments.iter().enumerate() {
        if index != 0 { buffer.write_char(' ').map_err(|_| "literal text exceeds 512 bytes")?; }
        buffer.write_str(argument).map_err(|_| "literal text exceeds 512 bytes")?;
    }
    Ok(buffer.text())
}

#[inline(never)]
fn single(operation: O, arguments: &[&str], raw: &str, resolved: Resolved,
    keyboard: &mut Keyboard, editor: &mut Editor) -> ExecState {
    match operation {
        O::Help => { help(arguments); return ExecState::Success; }
        O::Status => {
            let value = last();
            kprintln!("status: {} | {} | {}", code(value.state), value.operation, value.message);
            return ExecState::Success;
        }
        O::Let => return match variables().set(arguments[0], arguments[1]) {
            Ok(()) => { kprintln!("set {}", arguments[0]); ExecState::Success }, Err(message) => error(message),
        },
        O::Unset => return match variables().unset(arguments[0]) {
            Ok(()) => { kprintln!("unset {}", arguments[0]); ExecState::Success }, Err(message) => error(message),
        },
        O::History => {
            for index in 0..editor.history_len() { kprintln!("{} {}", index + 1, editor.history(index).unwrap_or("")); }
            return ExecState::Success;
        }
        O::Plan => return if arguments.is_empty() { show_plan() } else { stage_plan(arguments) },
        O::Show => return show_plan(),
        O::Apply => return apply_plan(arguments),
        O::Run => return run_script(arguments[0], keyboard, editor),
        O::Ops | O::Vars => {
            let state = source(operation, arguments, keyboard, editor);
            if !matches!(state, ExecState::Success) { return state; }
            return records::write_table(table(), crate::console()).map(|_| ExecState::Success).unwrap_or_else(record_error);
        }
        _ => {}
    }
    // This compatibility path follows the same resolver and full validation.
    // It preserves legacy spacing and output only for literal, old spellings.
    let legacy = !raw.bytes().any(|byte| matches!(byte, b'\'' | b'"' | b'$' | b'\\' | b'|'));
    if legacy && (resolved.argument_offset == 1 || operation == O::Ping) {
        let action = shell::parse(raw);
        if !matches!(action, Action::Unknown | Action::Help) {
            let state = crate::execute_action(action, keyboard);
            if operation == O::DropToUser && matches!(state, ExecState::Success) { lower_cleanup(editor); }
            return state;
        }
    }
    let mut body = Buffer::<512>::new();
    let action = match operation {
        O::About => Action::About, O::Memory => Action::Memory, O::Clear => Action::Clear,
        O::Halt => Action::Halt, O::Reboot => Action::Reboot, O::Uptime => Action::Uptime,
        O::Alloc => Action::Alloc, O::Security => Action::Security, O::Resources => Action::Resources,
        O::Audit => Action::Audit, O::DropToUser => Action::DropToUser, O::Disk => Action::Disk,
        O::Format => Action::Format, O::Cat => Action::Cat(Ok(arguments[0])),
        O::Remove => Action::Remove(Ok(arguments[0])), O::Calc => Action::Calc(calculate(arguments)),
        O::Sleep => Action::Sleep(unsigned(arguments[0])), O::Free => Action::Free(address(arguments[0])),
        O::Fault => Action::Fault(fault(arguments[0])), O::Spawn => Action::Spawn(task_kind(arguments[0])),
        O::Kill => Action::Kill(unsigned(arguments[0]).map(|value| value as u32)),
        O::Ping => Action::Ping(ping_request(arguments)),
        O::Echo => match joined(arguments, &mut body) { Ok(text) => Action::Echo(text), Err(message) => return error(message) },
        O::Write | O::Append => match joined(&arguments[1..], &mut body) {
            Ok(text) => Action::Write { name: arguments[0], text, append: operation == O::Append },
            Err(message) => return error(message),
        },
        O::Tasks | O::List | O::Network => {
            let state = source(operation, arguments, keyboard, editor);
            if !matches!(state, ExecState::Success) { return state; }
            return records::write_table(table(), crate::console()).map(|_| ExecState::Success).unwrap_or_else(record_error);
        }
        _ => return error("operation cannot execute without pipeline input"),
    };
    let state = crate::execute_action(action, keyboard);
    if operation == O::DropToUser && matches!(state, ExecState::Success) { lower_cleanup(editor); }
    state
}

fn lower_cleanup(editor: &mut Editor) {
    variables().clear();
    plan().discard();
    editor.clear_history();
    table().clear();
    remember(ExecState::Success, "drop", "domain lowered; shell state cleared");
}

fn text(value: &str) -> Result<Cell, records::Error> { Cell::text(value) }

fn text_rows(value: &str) -> Result<(), records::Error> {
    if value.is_empty() { return table().push_row(&[text("")?]); }
    let mut remaining = value;
    while !remaining.is_empty() {
        let mut end = remaining.len().min(records::MAX_TEXT);
        while !remaining.is_char_boundary(end) { end -= 1; }
        table().push_row(&[text(&remaining[..end])?])?;
        remaining = &remaining[end..];
    }
    Ok(())
}

#[inline(never)]
fn source(operation: O, arguments: &[&str], keyboard: &mut Keyboard, editor: &Editor) -> ExecState {
    if let Err(failure) = table().reset(schema(operation)) { return record_error(failure); }
    let result: Result<(), records::Error> = match operation {
        O::About => text_rows("Tane OS: original Rust kernel and typed shell; no heap, external crates or Linux.\n"),
        O::Echo => {
            let mut buffer = Buffer::<512>::new();
            match joined(arguments, &mut buffer) { Ok(value) => text_rows(value), Err(message) => return error(message) }
        }
        O::Cat => {
            let mut buffer = [0u8; fs::MAX_FILE_SIZE];
            let size = match storage::read(arguments[0], &mut buffer) { Ok(size) => size, Err(failure) => return storage_error(failure, false) };
            let value = match core::str::from_utf8(&buffer[..size]) { Ok(value) => value, Err(_) => return error("file is not UTF-8 text") };
            text_rows(value)
        }
        O::Calc => match calculate(arguments) {
            Ok(value) => table().push_row(&[Cell::Int(value)]),
            Err(message) => return error(message),
        },
        O::Uptime => {
            let ticks = interrupts::ticks();
            table().push_row(&[Cell::UInt(ticks.saturating_mul(1000) / interrupts::TIMER_HZ), Cell::UInt(ticks)])
        }
        O::Status => {
            let value = last();
            (|| { table().push_row(&[text(code(value.state))?, text(value.operation)?, text(value.message)?]) })()
        }
        O::Memory => {
            let (usable, free) = crate::with_frames(|frames| (frames.usable_frames(), frames.free_frames()));
            table().push_row(&[Cell::UInt(usable as u64), Cell::UInt(free as u64), Cell::UInt((usable - free) as u64), Cell::UInt(crate::frames::FRAME_SIZE)])
        }
        O::Tasks => {
            let mut result = Ok(());
            tasks::list(|task| {
                if result.is_ok() {
                    result = (|| { table().push_row(&[Cell::UInt(task.pid as u64), text(task.name)?, text(task.domain.name())?, text(task.state)?,
                        Cell::UInt(task.cpu_ticks), task.counter.map(Cell::UInt).unwrap_or(Cell::Null), Cell::UInt(task.stack)]) })();
                }
            });
            result
        }
        O::List => {
            let mut result = Ok(());
            let listed = storage::list(|file| {
                if result.is_ok() {
                    result = (|| { table().push_row(&[Cell::UInt(file.slot as u64), text(file.label.name())?, Cell::UInt(file.size as u64),
                        Cell::UInt(file.generation as u64), text(file.name)?]) })();
                }
            });
            if let Err(failure) = listed { return storage_error(failure, false); }
            result
        }
        O::Network => {
            if let Some(value) = net::status() {
                let (mut mac, mut ipv4, mut ipv6) = (Buffer::<64>::new(), Buffer::<64>::new(), Buffer::<64>::new());
                if write!(mac, "{}", inet::Mac(value.info.mac)).is_err()
                    || write!(ipv4, "{}", inet::IpAddr::V4(value.config.ipv4)).is_err()
                    || write!(ipv6, "{}", inet::IpAddr::V6(value.config.ipv6)).is_err() { return error("address exceeds text capacity"); }
                (|| { table().push_row(&[Cell::Bool(true), text(mac.text())?, text(ipv4.text())?, text(ipv6.text())?,
                    Cell::UInt(value.stats.rx), Cell::UInt(value.stats.tx), Cell::UInt(value.stats.dropped), Cell::UInt(value.neighbors as u64)]) })()
            } else { table().push_row(&[Cell::Bool(false), Cell::Null, Cell::Null, Cell::Null, Cell::UInt(0), Cell::UInt(0), Cell::UInt(0), Cell::UInt(0)]) }
        }
        O::Ping => return ping_source(arguments, keyboard),
        O::Security => (|| { table().push_row(&[text(tasks::current_domain().name())?, Cell::UInt(security::denials()), Cell::UInt(mac::POLICY.len() as u64)]) })(),
        O::Resources => (|| {
            for domain in [Domain::Admin, Domain::User] {
                let usage = security::usage(domain);
                let limit = resources::limits(domain);
                table().push_row(&[text(domain.name())?, Cell::UInt(usage.tasks as u64), Cell::UInt(usage.frames as u64),
                    storage::files_owned(domain).map(|value| Cell::UInt(value as u64)).unwrap_or(Cell::Null),
                    Cell::UInt(usage.last_window_ticks as u64), Cell::UInt(limit.cpu_percent as u64),
                    Cell::UInt(limit.tasks as u64), Cell::UInt(limit.frames as u64)])?;
            }
            Ok(())
        })(),
        O::Audit => {
            if let Err(denied) = security::check(Op::ReadAudit, None, None) { crate::report_denied(denied); return ExecState::Denied; }
            let mut result = Ok(());
            security::audit_records(|id, record| {
                if result.is_ok() {
                    result = (|| { table().push_row(&[Cell::UInt(id), Cell::UInt(record.tick), Cell::UInt(record.pid as u64), text(record.subject.name())?, text(record.op.name())?,
                        match record.object { Some(domain) => text(domain.name())?, None => Cell::Null }, record.target.map(Cell::UInt).unwrap_or(Cell::Null),
                        text(match record.reason { mac::Reason::Policy => "policy", mac::Reason::Quota => "quota" })?]) })();
                }
            });
            result
        }
        O::Disk => (|| { match storage::status() {
            storage::Status::Absent => table().push_row(&[Cell::Bool(false), Cell::Bool(false), Cell::Null, Cell::Null, Cell::Null, text("no disk")?]),
            storage::Status::Mounted { model, sectors, files } => table().push_row(&[Cell::Bool(true), Cell::Bool(true), text(model)?, Cell::UInt(sectors as u64), Cell::UInt(files as u64), Cell::Null]),
            storage::Status::Unformatted { model, sectors, error } => table().push_row(&[Cell::Bool(true), Cell::Bool(false), text(model)?, Cell::UInt(sectors as u64), Cell::Null, text(error.message())?]),
        } })(),
        O::Ops => (|| {
            for item in &operations::OPERATIONS { table().push_row(&[text(item.name)?, text(item.effect.name())?, text(item.schema.name())?, text(item.permission)?])?; }
            Ok(())
        })(),
        O::Vars => (|| {
            for index in 0..variable_view().len() {
                if let Some((name, value)) = variable_view().get(index) { table().push_row(&[text(name)?, text(value)?])?; }
            }
            Ok(())
        })(),
        O::History => (|| {
            for index in 0..editor.history_len() {
                let mut remaining = editor.history(index).unwrap_or("");
                let mut part = 1;
                if remaining.is_empty() { table().push_row(&[Cell::UInt((index + 1) as u64), Cell::UInt(part), text("")?])?; }
                while !remaining.is_empty() {
                    let mut end = remaining.len().min(records::MAX_TEXT);
                    while !remaining.is_char_boundary(end) { end -= 1; }
                    table().push_row(&[Cell::UInt((index + 1) as u64), Cell::UInt(part), text(&remaining[..end])?])?;
                    part += 1;
                    remaining = &remaining[end..];
                }
            }
            Ok(())
        })(),
        _ => return error("operation is not a record source"),
    };
    result.map(|_| ExecState::Success).unwrap_or_else(record_error)
}

fn ping_source(arguments: &[&str], keyboard: &mut Keyboard) -> ExecState {
    unsafe { *addr_of_mut!(PING_OUTCOME) = ExecState::Success; }
    let request = match ping_request(arguments) { Ok(request) => request, Err(message) => return error(message) };
    let target = match inet::parse(request.address) { Some(target) => target, None => return error("invalid IP address") };
    let mut rows = Ok(());
    let result = net::ping(target, request.count, request.timeout_ms, || crate::cancelled(keyboard), |event| {
        if rows.is_err() { return; }
        rows = (|| { match event {
            net::Event::Reply(reply) => {
                let mut address = Buffer::<64>::new();
                write!(address, "{}", reply.source).map_err(|_| records::Error::TextTooLong)?;
                table().push_row(&[text("reply")?, text(address.text())?, Cell::UInt(reply.sequence as u64),
                    Cell::UInt(reply.bytes as u64), Cell::UInt(reply.ttl as u64),
                    Cell::UInt(reply.rtt_ticks.saturating_mul(1000) / interrupts::TIMER_HZ), Cell::Null, Cell::Null])
            }
            net::Event::Timeout { sequence } => table().push_row(&[text("timeout")?, text(request.address)?, Cell::UInt(sequence as u64), Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null]),
        } })();
    });
    if let Err(failure) = rows { return record_error(failure); }
    match result {
        Ok(summary) => {
            let cancelled = summary.completion == net::Completion::Cancelled;
            let kind = if cancelled { "cancelled" } else { "summary" };
            let row = (|| { table().push_row(&[text(kind)?, text(request.address)?, Cell::Null, Cell::Null, Cell::Null, Cell::Null,
                Cell::UInt(summary.sent as u64), Cell::UInt(summary.received as u64)]) })();
            if let Err(failure) = row { return record_error(failure); }
            if cancelled { kprintln!("cancelled"); ExecState::Cancelled }
            else {
                // Typed timeout rows still reach transforms/renderers, while
                // command and script status agrees with the legacy probe.
                if summary.received != summary.sent { unsafe { *addr_of_mut!(PING_OUTCOME) = ExecState::Error; } }
                ExecState::Success
            }
        }
        Err(net::Error::Denied(denied)) => { crate::report_denied(denied); ExecState::Denied }
        Err(net::Error::Absent) => error("network unavailable (no RTL8139 detected)"),
        Err(net::Error::InvalidRequest) => error("invalid ping bounds"),
        Err(net::Error::Device(failure)) => error(failure.message()),
        Err(net::Error::Protocol(failure)) => error(failure.message()),
    }
}

#[inline(never)]
fn save_result(name: &str, source: O, json: bool, untransformed: bool) -> ExecState {
    let mut buffer = Buffer::<{ fs::MAX_FILE_SIZE }>::new();
    let result = if json { records::write_json(table(), &mut buffer) }
        else if untransformed && matches!(source, O::Echo | O::Cat | O::About) {
            let mut result = Ok(());
            for index in 0..table().len() {
                if let Some(Cell::Text(value)) = table().get(index, 0) {
                    if buffer.write_str(value.as_str()).is_err() { result = Err(records::Error::Write); break; }
                }
            }
            if source == O::Echo && result.is_ok() && buffer.write_char('\n').is_err() { result = Err(records::Error::Write); }
            result
        } else if untransformed && source == O::Calc {
            match table().get(0, 0) {
                Some(Cell::Int(value)) => writeln!(buffer, "{}", value).map_err(|_| records::Error::Write),
                _ => Err(records::Error::TypeMismatch),
            }
        } else { records::write_tsv(table(), &mut buffer) };
    if result.is_err() { return error("rendered output exceeds 4096 bytes; file unchanged"); }
    if let Err(failure) = storage::preflight(name, PlannedKind::Write, buffer.len) { return storage_error(failure, false); }
    match storage::write(name, &buffer.bytes[..buffer.len], false) {
        Ok(created) => { kprintln!("{} {} ({} bytes)", if created { "created" } else { "wrote" }, name, buffer.len); ExecState::Success }
        Err(failure) => storage_error(failure, true),
    }
}

pub(crate) fn help(arguments: &[&str]) {
    if arguments.is_empty() {
        kprintln!("Tane shell: literal words, typed records, checked effects.");
        for item in &operations::OPERATIONS { kprintln!("  {:<24} {}", item.usage, item.description); }
        kprintln!("Quotes: 'literal', \"$NAME\", \\n/\\t. Pipes carry typed records; expansion never runs code.");
        kprintln!("Bounds: 255 input bytes, 8 stages, 48 words, 64 rows, 8 columns, 64 text bytes.");
        return;
    }
    if let Some(resolved) = operations::resolve(arguments[0], arguments.get(1).copied()) {
        let item = operations::metadata(resolved.operation);
        kprintln!("{}\n{}\neffect: {} | result: {} | permission: {}\nexample: {}", item.usage, item.description,
            item.effect.name(), item.schema.name(), item.permission, item.example);
        if !item.aliases.is_empty() { kprintln!("aliases: {:?}", item.aliases); }
    }
}

#[inline(never)]
fn stage_plan(arguments: &[&str]) -> ExecState {
    let target = match operations::resolve(arguments[0], arguments.get(1).copied()) { Some(target) => target, None => return error("unknown planned operation") };
    let arguments = &arguments[target.argument_offset..];
    let kind = match target.operation { O::Write => PlannedKind::Write, O::Append => PlannedKind::Append, O::Remove => PlannedKind::Remove,
        _ => return error("only one file write, append or remove may be planned") };
    let mut buffer = Buffer::<512>::new();
    let payload = if kind == PlannedKind::Remove { "" }
        else { match joined(&arguments[1..], &mut buffer) { Ok(text) => text, Err(message) => return error(message) } };
    if let Err(failure) = storage::preflight(arguments[0], kind, payload.len()) { return storage_error(failure, false); }
    let before = match storage::snapshot(arguments[0]) { Ok(before) => before, Err(failure) => return storage_error(failure, false) };
    match plan().stage(kind, arguments[0], payload.as_bytes(), tasks::current_domain(), storage::revision(), before) {
        Ok(id) => { kprintln!("planned #{}; show to review, apply {} to commit", id, id); ExecState::Success }
        Err(failure) => error(failure.message()),
    }
}

fn show_plan() -> ExecState {
    let view = match plan().show_for(tasks::current_domain()) { Ok(view) => view, Err(failure) => return error(failure.message()) };
    kprintln!("plan #{}: file {} {} | {} bytes | domain {} | revision {}", view.id, view.kind.name(), view.name,
        view.payload.len(), view.creator.name(), view.revision);
    kprintln!("required: {}{}", view.required_op().name(),
        if matches!(view.before, crate::plans::Snapshot::Exists { .. }) { " + read target for identity verification" } else { "" });
    match view.before {
        crate::plans::Snapshot::Absent => kprintln!("before: absent"),
        crate::plans::Snapshot::Exists { slot, label, size, generation, checksum } =>
            kprintln!("before: slot {} | label {} | {} bytes | generation {} | checksum {:08x}", slot, label.name(), size, generation, checksum),
    }
    write!(crate::console(), "payload: \"").ok();
    for &byte in view.payload {
        match byte {
            b'\n' => write!(crate::console(), "\\n").ok(), b'\r' => write!(crate::console(), "\\r").ok(),
            b'\t' => write!(crate::console(), "\\t").ok(), b'\\' => write!(crate::console(), "\\\\").ok(),
            b'"' => write!(crate::console(), "\\\"").ok(),
            b' '..=b'~' => crate::console().write_char(byte as char).ok(),
            byte => write!(crate::console(), "\\x{:02x}", byte).ok(),
        };
    }
    kprintln!("\"");
    ExecState::Success
}

#[inline(never)]
fn apply_plan(arguments: &[&str]) -> ExecState {
    let id = match arguments.first() { Some(value) => unsigned(value).unwrap_or(0), None => plan().id().unwrap_or(0) };
    let domain = tasks::current_domain();
    let mut name = Buffer::<{ fs::MAX_NAME }>::new();
    let details = plan().show_for(domain).map(|view| {
        let _ = name.write_str(view.name);
        (view.kind, view.payload.len())
    });
    let observed = match details {
        Ok((kind, size)) => {
            if let Err(failure) = storage::preflight(name.text(), kind, size) {
                let _ = plan().begin_apply(id, domain, storage::revision(), None);
                plan().discard();
                return storage_error(failure, false);
            }
            match storage::snapshot(name.text()) {
                Ok(snapshot) => Some(snapshot),
                Err(failure) => {
                    let _ = plan().begin_apply(id, domain, storage::revision(), None);
                    plan().discard();
                    return storage_error(failure, false);
                }
            }
        }
        Err(_) => None,
    };
    let view = match plan().begin_apply(id, domain, storage::revision(), observed) {
        Ok(view) => view,
        Err(failure) => { plan().discard(); return error(failure.message()); }
    };
    let result = match view.kind {
        PlannedKind::Write => storage::write(view.name, view.payload, false).map(|_| ()),
        PlannedKind::Append => storage::write(view.name, view.payload, true).map(|_| ()),
        PlannedKind::Remove => storage::remove(view.name),
    };
    plan().discard();
    match result {
        Ok(()) => { kprintln!("applied #{}: committed", id); ExecState::Success },
        Err(failure) => storage_error(failure, true),
    }
}

struct ScriptBuffer { bytes: [u8; fs::MAX_FILE_SIZE] }

impl Drop for ScriptBuffer {
    fn drop(&mut self) {
        // Volatile stores keep erasure observable even when the stack frame
        // is about to disappear. All exit paths, including prescan failures,
        // erase a previously loaded privileged script's backing bytes.
        for byte in &mut self.bytes { unsafe { core::ptr::write_volatile(byte, 0); } }
    }
}

#[inline(never)]
fn run_script(name: &str, keyboard: &mut Keyboard, editor: &mut Editor) -> ExecState {
    let depth = unsafe { *addr_of_mut!(SCRIPT_DEPTH) };
    if depth == 4 { return error("script nesting exceeds 4"); }
    let creator = tasks::current_domain();
    let mut storage_buffer = ScriptBuffer { bytes: [0; fs::MAX_FILE_SIZE] };
    let bytes = &mut storage_buffer.bytes;
    let size = match storage::read(name, bytes) { Ok(size) => size, Err(failure) => return storage_error(failure, false) };
    if !bytes[..size].iter().all(|byte| matches!(byte, b'\n' | b'\r' | b'\t' | b' '..=b'~')) { return error("script must contain ASCII source lines"); }
    let script = core::str::from_utf8(&bytes[..size]).unwrap_or("");
    let mut lines = 0;
    // Parse the complete file first: malformed syntax and unknown operation
    // names cannot hide after an earlier committing line. Dynamic argument
    // types are deliberately checked against their actual values at runtime.
    for line in script.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let pipeline = match shell_lang::parse(line, |_| Some("0")) {
            Ok(pipeline) => pipeline, Err(failure) => { kprintln!("error: script syntax: {}", failure.message()); return ExecState::Error; }
        };
        if pipeline.stage_count() == 0 { continue; }
        lines += 1;
        if lines > 32 { return error("script exceeds 32 command lines"); }
        for stage in 0..pipeline.stage_count() {
            let resolved = match resolve(&pipeline, stage) { Ok(resolved) => resolved, Err(message) => return error(message) };
            if resolved.operation == O::Plan && pipeline.word_count(stage) > resolved.argument_offset {
                let first = resolved.argument_offset;
                if pipeline.word_expanded(stage, first) { return error("planned operation names must be literal"); }
                let target = match operations::resolve(word(&pipeline, stage, first), pipeline.word(stage, first + 1)) {
                    Some(target) => target, None => return error("unknown planned operation"),
                };
                if target.argument_offset == 2 && pipeline.word_expanded(stage, first + 1) {
                    return error("planned subcommand names must be literal");
                }
                if !matches!(target.operation, O::Write | O::Append | O::Remove) {
                    return error("only one file write, append or remove may be planned");
                }
            }
        }
    }
    unsafe { *addr_of_mut!(SCRIPT_DEPTH) += 1; }
    let mut result = ExecState::Success;
    for (index, line) in script.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim_ascii().is_empty() || line.trim_ascii().starts_with('#') { continue; }
        if crate::cancelled(keyboard) { kprintln!("cancelled: script {}", name); result = ExecState::Cancelled; break; }
        let budget = unsafe { &mut *addr_of_mut!(SCRIPT_BUDGET) };
        if *budget == 0 { result = error("script execution exceeds 128 total lines"); break; }
        *budget -= 1;
        result = execute_line(line, keyboard, editor);
        net::poll();
        if tasks::current_domain() != creator {
            result = error("script stopped after domain transition");
            break;
        }
        if !matches!(result, ExecState::Success) { kprintln!("stopped: {} line {}", name, index + 1); break; }
    }
    unsafe { *addr_of_mut!(SCRIPT_DEPTH) -= 1; }
    // ScriptBuffer erases all bytes as this invocation returns.
    result
}

/// Completion uses catalog metadata and the storage visibility gate. It only
/// changes the editor or prints choices; it never executes a candidate.
pub fn complete(editor: &mut Editor) -> bool {
    if editor.cursor() != editor.length() { return false; }
    let mut line = Buffer::<255>::new();
    if line.write_str(editor.line()).is_err() { return false; }
    let prefix = line.text();
    if prefix.bytes().any(|byte| matches!(byte, b'\'' | b'"' | b'\\' | b'|')) { return false; }
    let mut candidates: [&str; 64] = [""; 64];
    let mut count = 0;
    for item in &operations::OPERATIONS {
        if item.name.starts_with(prefix) && count < candidates.len() { candidates[count] = item.name; count += 1; }
        for alias in item.aliases {
            if alias.starts_with(prefix) && count < candidates.len() { candidates[count] = alias; count += 1; }
        }
    }
    if count > 0 { return complete_choices(editor, "", &candidates[..count]); }
    let split = prefix.rfind(' ').map(|index| index + 1).unwrap_or(0);
    let partial = &prefix[split..];
    if let Some(partial) = partial.strip_prefix('$') {
        for index in 0..variable_view().len() {
            if let Some((name, _)) = variable_view().get(index) {
                if name.starts_with(partial) && count < candidates.len() { candidates[count] = name; count += 1; }
            }
        }
        if "STATUS".starts_with(partial) { candidates[count] = "STATUS"; count += 1; }
        return complete_choices(editor, &prefix[..split + 1], &candidates[..count]);
    }
    let mut first = prefix[..split].split_ascii_whitespace();
    let head = first.next().unwrap_or("");
    let second = first.next();
    let resolved = match operations::resolve(head, second) { Some(resolved) => resolved, None => return false };
    if !matches!(resolved.operation, O::Cat | O::Write | O::Append | O::Remove | O::Run | O::Save)
        || prefix[..split].split_ascii_whitespace().count() != resolved.argument_offset { return false; }
    // Storage names are borrowed only during list; copy them into bounded
    // completion storage so no filesystem borrow escapes into later edits.
    let mut names = [[0u8; fs::MAX_NAME]; fs::MAX_FILES];
    let mut lengths = [0usize; fs::MAX_FILES];
    let listed = storage::list(|file| {
        if file.name.starts_with(partial) && count < fs::MAX_FILES {
            names[count][..file.name.len()].copy_from_slice(file.name.as_bytes());
            lengths[count] = file.name.len();
            count += 1;
        }
    });
    if listed.is_err() { return false; }
    for index in 0..count { candidates[index] = core::str::from_utf8(&names[index][..lengths[index]]).unwrap_or(""); }
    complete_choices(editor, &prefix[..split], &candidates[..count])
}

fn complete_choices(editor: &mut Editor, before: &str, candidates: &[&str]) -> bool {
    if candidates.is_empty() { return false; }
    if candidates.len() == 1 {
        let mut buffer = Buffer::<255>::new();
        if write!(buffer, "{}{} ", before, candidates[0]).is_ok() { let _ = editor.set_line(buffer.text()); }
        return false;
    }
    let mut common = candidates[0].len();
    for candidate in &candidates[1..] {
        common = candidates[0].as_bytes()[..common].iter().zip(candidate.as_bytes()).take_while(|(left, right)| left == right).count();
    }
    let existing = editor.line().len().saturating_sub(before.len());
    if common > existing {
        let mut buffer = Buffer::<255>::new();
        if write!(buffer, "{}{}", before, &candidates[0][..common]).is_ok() { let _ = editor.set_line(buffer.text()); }
        return false;
    }
    kprintln!();
    for candidate in candidates { kprintln!("  {}", candidate); }
    true
}
