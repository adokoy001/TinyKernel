//! Single-consumer network service. Only the shell task calls this module;
//! IRQs and other tasks never borrow the stack or print network messages.
//! The protocol engine returns typed results and never writes to a console.

use crate::{inet::IpAddr, interrupts, mac::Op, netstack, rtl8139, security, tasks};
use core::ptr::addr_of_mut;

#[derive(Clone, Copy, Default)]
pub struct Stats { pub rx: u64, pub tx: u64, pub dropped: u64 }

pub struct Status { pub info: rtl8139::Info, pub config: netstack::Config, pub stats: Stats, pub neighbors: usize }

static mut STACK: Option<netstack::Stack> = None;
static mut INFO: Option<rtl8139::Info> = None;
static mut STATS: Stats = Stats { rx: 0, tx: 0, dropped: 0 };
static mut NEXT_ID: u16 = 0;

pub fn init() -> Result<rtl8139::Info, rtl8139::Error> {
    let info = rtl8139::init()?;
    unsafe {
        *addr_of_mut!(STACK) = Some(netstack::Stack::new(netstack::Config::qemu(info.mac)));
        *addr_of_mut!(INFO) = Some(info);
    }
    Ok(info)
}

pub fn present() -> bool { unsafe { (*addr_of_mut!(INFO)).is_some() } }

pub fn status() -> Option<Status> {
    unsafe {
        let stack = (*addr_of_mut!(STACK)).as_ref()?;
        Some(Status { info: (*addr_of_mut!(INFO))?, config: *stack.config(), stats: *addr_of_mut!(STATS),
            neighbors: stack.neighbors().iter().flatten().count() })
    }
}

pub enum Error {
    Absent,
    InvalidRequest,
    Denied(security::Denied),
    Device(rtl8139::Error),
    Protocol(netstack::Error),
}

pub enum Event { Reply(netstack::Reply), Timeout { sequence: u16 } }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion { Complete, Cancelled }

pub struct Summary { pub sent: u8, pub received: u8, pub completion: Completion }

fn send(frame: &[u8]) -> Result<(), Error> {
    rtl8139::send(frame).map_err(Error::Device)?;
    unsafe { (*addr_of_mut!(STATS)).tx = (*addr_of_mut!(STATS)).tx.saturating_add(1); }
    Ok(())
}

/// Limit work even when a peer floods the receive ring. Bad frames are data,
/// not fatal errors; device and transmission failures have separate results.
fn receive(stack: &mut netstack::Stack, now: u64, input: &mut [u8], output: &mut [u8]) -> Result<Option<netstack::Reply>, Error> {
    for _ in 0..16 {
        let length = match rtl8139::receive(input) {
            Ok(Some(length)) => length,
            Ok(None) => break,
            Err(error) => {
                unsafe { (*addr_of_mut!(STATS)).dropped = (*addr_of_mut!(STATS)).dropped.saturating_add(1); }
                match error {
                    rtl8139::Error::ReceiveCorrupt | rtl8139::Error::ReceiveBadStatus
                    | rtl8139::Error::ReceiveOverflow | rtl8139::Error::BufferTooSmall => continue,
                    _ => return Err(Error::Device(error)),
                }
            }
        };
        unsafe { (*addr_of_mut!(STATS)).rx = (*addr_of_mut!(STATS)).rx.saturating_add(1); }
        match stack.ingest(&input[..length], now, output) {
            Ok(outcome) => {
                if outcome.tx_len > 0 { send(&output[..outcome.tx_len])?; }
                if outcome.reply.is_some() { return Ok(outcome.reply); }
            }
            Err(_) => unsafe { (*addr_of_mut!(STATS)).dropped = (*addr_of_mut!(STATS)).dropped.saturating_add(1); },
        }
    }
    Ok(None)
}

/// Answer ARP/NDP/echo while the prompt is idle. Never runs in an IRQ.
pub fn poll() {
    let mut input = [0u8; netstack::MAX_FRAME];
    let mut output = [0u8; netstack::MAX_FRAME];
    if let Some(stack) = unsafe { (*addr_of_mut!(STACK)).as_mut() } {
        let _ = receive(stack, interrupts::ticks(), &mut input, &mut output);
    }
}

/// One bounded job with a deterministic authorization check before any
/// requested ARP, NDP or echo packet is emitted. Callbacks only render typed
/// events or read cancellation; they never borrow the protocol stack.
pub fn ping(target: IpAddr, count: u8, timeout_ms: u64, mut cancelled: impl FnMut() -> bool,
            mut event: impl FnMut(Event)) -> Result<Summary, Error> {
    if !(1..=10).contains(&count) || !(1..=5000).contains(&timeout_ms) { return Err(Error::InvalidRequest); }
    security::check(Op::NetPing, None, None).map_err(Error::Denied)?;
    if target.is_unspecified() || target.is_multicast() {
        return Err(Error::Protocol(netstack::Error::InvalidTarget));
    }
    let stack = unsafe { (*addr_of_mut!(STACK)).as_mut() }.ok_or(Error::Absent)?;
    let identifier = unsafe {
        let id = addr_of_mut!(NEXT_ID);
        *id = (*id).wrapping_add(1);
        *id
    };
    let timeout = (timeout_ms * interrupts::TIMER_HZ).div_ceil(1000);
    let mut input = [0u8; netstack::MAX_FRAME];
    let mut output = [0u8; netstack::MAX_FRAME];
    let result = (|| {
        let mut summary = Summary { sent: 0, received: 0, completion: Completion::Complete };
        for sequence in 1..=count as u16 {
            if cancelled() { summary.completion = Completion::Cancelled; break; }
            let length = stack.start_ping(target, identifier, sequence, interrupts::ticks(), &mut output).map_err(Error::Protocol)?;
            send(&output[..length])?;
            summary.sent += 1;
            loop {
                if cancelled() { summary.completion = Completion::Cancelled; return Ok(summary); }
                let now = interrupts::ticks();
                // Check the overall deadline before each bounded batch.
                // Timing uses the PIT's 10 ms resolution.
                match stack.poll(now, timeout, &mut output).map_err(Error::Protocol)? {
                    netstack::Poll::Send(length) => send(&output[..length])?,
                    netstack::Poll::Timeout => { event(Event::Timeout { sequence }); break; }
                    netstack::Poll::Idle => {}
                }
                if let Some(reply) = receive(stack, now, &mut input, &mut output)? {
                    summary.received += 1;
                    event(Event::Reply(reply));
                    break;
                }
                tasks::sleep_until(now.saturating_add(1));
            }
        }
        Ok(summary)
    })();
    stack.cancel_ping();
    result
}
