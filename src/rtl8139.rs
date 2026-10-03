//! Original RTL8139 legacy-mode DMA driver for QEMU `-device rtl8139`.
//! IRQs are masked; bounded polling leaves the kernel's timer enabled.
//! One device, coherent x86 DMA, physical == virtual addresses below 1 GiB.
//! No C+ mode, heap allocation, VLAN, jumbo frames or link negotiation code.
//!
//! Register/ownership/ring rules were checked against the Realtek RTL8139C
//! datasheet revision 1.6 (sections 5/6) and QEMU's rtl8139.c, not copied from
//! another driver:
//! https://people.freebsd.org/~wpaul/RealTek/spec-8139c(160).pdf
//! https://qemu.googlesource.com/qemu/+/refs/tags/v8.1.2/hw/net/rtl8139.c

pub const MAX_FRAME: usize = 1514; // Ethernet header + 1500-byte MTU, no FCS
const RX_RING: usize = 8192;
const RX_BYTES: usize = RX_RING + 16 + 2048;
const TX_BYTES: usize = 1536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub mac: [u8; 6],
    pub io: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NotFound,
    InvalidBar,
    PciCommand,
    InvalidDmaAddress,
    InvalidMac,
    NotInitialized,
    Busy,
    ResetTimeout,
    TransmitBusy,
    TransmitTimeout,
    TransmitFailed,
    InvalidFrameLength,
    ReceiveCorrupt,
    ReceiveBadStatus,
    ReceiveOverflow,
    BufferTooSmall,
    DeviceError,
}

impl Error {
    pub fn message(self) -> &'static str {
        match self {
            Self::NotFound => "RTL8139 PCI device not found",
            Self::InvalidBar => "RTL8139 needs an assigned 256-byte I/O BAR",
            Self::PciCommand => "PCI I/O/bus-master enable failed",
            Self::InvalidDmaAddress => "NIC DMA buffers are outside low identity-mapped RAM",
            Self::InvalidMac => "NIC returned an invalid MAC address",
            Self::NotInitialized => "network device is not initialized",
            Self::Busy => "network driver is in use",
            Self::ResetTimeout => "NIC reset timed out",
            Self::TransmitBusy => "NIC transmit descriptor is busy",
            Self::TransmitTimeout => "NIC transmit timed out",
            Self::TransmitFailed => "NIC transmit failed",
            Self::InvalidFrameLength => "Ethernet frame must be 14-1514 bytes without FCS",
            Self::ReceiveCorrupt => "invalid receive length; receive ring reset",
            Self::ReceiveBadStatus => "bad receive status; packet dropped",
            Self::ReceiveOverflow => "receive ring overflow; NIC reset",
            Self::BufferTooSmall => "receive buffer too small; packet dropped",
            Self::DeviceError => "NIC PCI bus error",
        }
    }
}

/// Length in the RTL header includes the 4-byte FCS, not its own header.
fn frame_length(wire_length: usize) -> Option<usize> {
    (64..=MAX_FRAME + 4).contains(&wire_length).then_some(wire_length.saturating_sub(4))
}

/// Every next record begins on a dword boundary, modulo the ring size.
fn next_rx(offset: usize, wire_length: usize) -> usize {
    (offset + 4 + wire_length + 3) & !3 & (RX_RING - 1)
}

fn capr(offset: usize) -> u16 {
    (offset as u16).wrapping_sub(16)
}

fn valid_dma(address: usize, bytes: usize) -> bool {
    address >= 0x10000 && address & 0xff == 0
        && address.checked_add(bytes).is_some_and(|end| end <= 1 << 30)
}

#[cfg(not(test))]
mod hardware {
    use super::*;
    use core::arch::asm;
    use core::ptr::{addr_of_mut, read_volatile, write_volatile};
    use core::sync::atomic::{fence, AtomicBool, Ordering};

    const CMD: u16 = 0x37;
    const CAPR: u16 = 0x38;
    const IMR: u16 = 0x3c;
    const ISR: u16 = 0x3e;
    const RCR: u16 = 0x44;
    const POLL_LIMIT: usize = 100_000;
    const TX_OWN: u32 = 1 << 13;
    const TX_OK: u32 = 1 << 15;
    const TX_ERRORS: u32 = (1 << 14) | (1 << 29) | (1 << 30) | (1 << 31);

    #[repr(C, align(256))]
    struct ReceiveBuffer([u8; RX_BYTES]);
    #[repr(C, align(256))]
    struct TransmitBuffer([u8; TX_BYTES]);
    static mut RX: ReceiveBuffer = ReceiveBuffer([0; RX_BYTES]);
    static mut TX: [TransmitBuffer; 4] = [const { TransmitBuffer([0; TX_BYTES]) }; 4];

    struct State { io: u16, rx: usize, tx: usize }
    static mut STATE: State = State { io: 0, rx: 0, tx: 0 };
    static IN_USE: AtomicBool = AtomicBool::new(false);

    struct Guard;
    impl Guard {
        fn take() -> Result<Self, Error> {
            IN_USE.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .map(|_| Guard).map_err(|_| Error::Busy)
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) { IN_USE.store(false, Ordering::Release); }
    }

    unsafe fn inw(port: u16) -> u16 {
        let value: u16;
        asm!("in ax, dx", in("dx") port, out("ax") value, options(nomem, nostack, preserves_flags));
        value
    }
    unsafe fn outw(port: u16, value: u16) {
        asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags));
    }
    unsafe fn inl(port: u16) -> u32 {
        let value: u32;
        asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack, preserves_flags));
        value
    }
    unsafe fn outl(port: u16, value: u32) {
        asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
    }
    unsafe fn rx_pointer() -> *mut u8 { addr_of_mut!(RX.0).cast() }
    unsafe fn tx_pointer(index: usize) -> *mut u8 {
        addr_of_mut!((*addr_of_mut!(TX).cast::<TransmitBuffer>().add(index)).0).cast()
    }

    unsafe fn configure(state: &mut State) -> Result<(), Error> {
        let io = state.io;
        outw(io + IMR, 0);
        crate::outb(io + CMD, 0x10); // Reset: DMA/receive/transmit stop first.
        let mut reset = false;
        for _ in 0..POLL_LIMIT {
            if crate::inb(io + CMD) & 0x10 == 0 { reset = true; break; }
            core::hint::spin_loop();
        }
        if !reset {
            crate::outb(io + CMD, 0);
            return Err(Error::ResetTimeout);
        }
        // CONFIG1 is protected by the 9346CR configuration-write gate.
        crate::outb(io + 0x50, 0xc0);
        crate::outb(io + 0x52, 0);
        crate::outb(io + 0x50, 0);
        for byte in 0..RX_BYTES { write_volatile(rx_pointer().add(byte), 0); }
        for index in 0..4 {
            let pointer = tx_pointer(index);
            for byte in 0..TX_BYTES { write_volatile(pointer.add(byte), 0); }
            outl(io + 0x20 + index as u16 * 4, pointer as u32);
        }
        fence(Ordering::SeqCst);
        outl(io + 0x30, rx_pointer() as u32);
        // IPv6 NDP uses 33:33 multicast. This small driver admits all
        // multicast hashes; the protocol layer validates destination groups.
        outl(io + 0x08, 0xffff_ffff);
        outl(io + 0x0c, 0xffff_ffff);
        state.rx = 0;
        state.tx = 0;
        outw(io + CAPR, capr(0));
        outw(io + ISR, 0xffff);
        crate::outb(io + CMD, 0x0c); // Enable Tx/Rx before threshold config.
        outl(io + 0x40, 0x0300_0700); // Standard IFG; 2048-byte Tx DMA burst.
        // 8 KiB, whole-packet Rx FIFO threshold, unlimited DMA, WRAP=1:
        // a record crossing the end is contiguous in the reserved tail.
        // Own unicast, multicast and broadcasts; no bad/runt/promiscuous.
        outl(io + RCR, 0x0000_e78e);
        outw(io + IMR, 0);
        Ok(())
    }

    /// Reset after malformed DMA metadata/overflow or a stuck transmitter.
    /// A failed recovery makes the device unavailable until explicit init.
    unsafe fn recover(state: &mut State) {
        if configure(state).is_err() { state.io = 0; }
    }

    pub fn init() -> Result<Info, Error> {
        let _guard = Guard::take()?;
        let state = unsafe { &mut *addr_of_mut!(STATE) };
        if state.io != 0 { unsafe { crate::outb(state.io + CMD, 0); outw(state.io + IMR, 0); } }
        state.io = 0;
        let address = crate::pci::find(0x10ec, 0x8139).ok_or(Error::NotFound)?;
        let io = crate::pci::rtl8139_io_bar(crate::pci::read32(address, 0x10)).ok_or(Error::InvalidBar)?;
        unsafe {
            if !valid_dma(rx_pointer() as usize, RX_BYTES)
                || (0..4).any(|index| !valid_dma(tx_pointer(index) as usize, TX_BYTES)) {
                return Err(Error::InvalidDmaAddress);
            }
        }
        if !crate::pci::enable_io_bus_master(address) { return Err(Error::PciCommand); }
        state.io = io;
        if let Err(error) = unsafe { configure(state) } { state.io = 0; return Err(error); }
        let mut mac = [0; 6];
        for (index, byte) in mac.iter_mut().enumerate() { *byte = unsafe { crate::inb(io + index as u16) }; }
        if mac == [0; 6] || mac[0] & 1 != 0 {
            unsafe { crate::outb(io + CMD, 0); }
            state.io = 0;
            return Err(Error::InvalidMac);
        }
        Ok(Info { mac, io })
    }

    /// Poll at most one record. Successful lengths exclude the FCS.
    /// Bad packets or a too-small output buffer are consumed and reported;
    /// invalid ring lengths/overflow cause a bounded reset, dropping the queue.
    pub fn receive(output: &mut [u8]) -> Result<Option<usize>, Error> {
        let _guard = Guard::take()?;
        let state = unsafe { &mut *addr_of_mut!(STATE) };
        if state.io == 0 { return Err(Error::NotInitialized); }
        unsafe {
            let interrupt_status = inw(state.io + ISR);
            if interrupt_status & 0x8050 != 0 {
                let error = if interrupt_status & 0x8000 != 0 { Error::DeviceError } else { Error::ReceiveOverflow };
                recover(state);
                return Err(error);
            }
            if crate::inb(state.io + CMD) & 1 != 0 { return Ok(None); }
            fence(Ordering::Acquire);
            let pointer = rx_pointer().add(state.rx);
            let header = [read_volatile(pointer), read_volatile(pointer.add(1)),
                          read_volatile(pointer.add(2)), read_volatile(pointer.add(3))];
            let status = u16::from_le_bytes([header[0], header[1]]);
            let wire = u16::from_le_bytes([header[2], header[3]]) as usize;
            if wire == 0xfff0 { return Ok(None); } // Hardware's early-DMA marker.
            let Some(length) = frame_length(wire) else {
                recover(state);
                return Err(Error::ReceiveCorrupt);
            };
            let error = if status & 1 == 0 || status & 0x3e != 0 { Some(Error::ReceiveBadStatus) }
                else if length > output.len() { Some(Error::BufferTooSmall) } else { None };
            if error.is_none() {
                // WRAP=1 gives contiguous bytes through the extra tail reserve.
                for (index, byte) in output[..length].iter_mut().enumerate() {
                    *byte = read_volatile(pointer.add(4 + index));
                }
            }
            state.rx = next_rx(state.rx, wire);
            fence(Ordering::SeqCst); // Finish reading before handing storage back.
            outw(state.io + CAPR, capr(state.rx));
            // Ack only RxOK/RxErr; avoid clearing a concurrent overflow/error.
            outw(state.io + ISR, interrupt_status & 3);
            if let Some(error) = error { Err(error) } else { Ok(Some(length)) }
        }
    }

    /// Submit one frame and wait for bounded completion. A successful result
    /// means the NIC finished this descriptor, not that a peer received it.
    pub fn send(frame: &[u8]) -> Result<(), Error> {
        if !(14..=MAX_FRAME).contains(&frame.len()) { return Err(Error::InvalidFrameLength); }
        let _guard = Guard::take()?;
        let state = unsafe { &mut *addr_of_mut!(STATE) };
        if state.io == 0 { return Err(Error::NotInitialized); }
        unsafe {
            let descriptor = state.io + 0x10 + state.tx as u16 * 4;
            if inl(descriptor) & TX_OWN == 0 { return Err(Error::TransmitBusy); }
            let pointer = tx_pointer(state.tx);
            let length = frame.len().max(60);
            for index in 0..length {
                write_volatile(pointer.add(index), frame.get(index).copied().unwrap_or(0));
            }
            fence(Ordering::SeqCst); // Publish all bytes before clearing OWN.
            outl(descriptor, length as u32 | (48 << 16)); // 1536-byte FIFO threshold.
            for _ in 0..POLL_LIMIT {
                let status = inl(descriptor);
                if status & TX_ERRORS != 0 {
                    recover(state);
                    return Err(Error::TransmitFailed);
                }
                if status & (TX_OWN | TX_OK) == TX_OWN | TX_OK {
                    fence(Ordering::Acquire);
                    state.tx = (state.tx + 1) % 4;
                    outw(state.io + ISR, 4);
                    return Ok(());
                }
                core::hint::spin_loop();
            }
            recover(state);
            Err(Error::TransmitTimeout)
        }
    }
}

#[cfg(not(test))]
pub use hardware::{init, receive, send};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_include_crc_but_not_record_header() {
        assert_eq!(frame_length(64), Some(60));
        assert_eq!(frame_length(1518), Some(1514));
        for length in [0, 4, 63, 1519, 0xfff0, 0xffff] { assert_eq!(frame_length(length), None); }
    }

    #[test]
    fn ring_advance_and_hardware_capr_agree_across_wrap() {
        assert_eq!(next_rx(0, 64), 68);
        assert_eq!(next_rx(0, 65), 72);
        assert_eq!(next_rx(8188, 64), 64);
        assert_eq!(next_rx(6672, 1518), 4);
        assert_eq!(capr(0), 0xfff0);
        for offset in (0..RX_RING).step_by(4) {
            for wire in [64, 65, 1518] {
                let next = next_rx(offset, wire);
                assert_eq!(next % 4, 0);
                assert!(next < RX_RING);
                assert_eq!((capr(next) as usize + 16) % RX_RING, next);
                assert!(offset + 4 + wire <= RX_BYTES);
            }
        }
    }

    #[test]
    fn dma_address_bounds_do_not_truncate_or_wrap() {
        assert!(valid_dma(0x10000, RX_BYTES));
        assert!(valid_dma((1 << 30) - 0x100, 0x100));
        for address in [0, 0x10001, 1 << 30, usize::MAX - 255] {
            assert!(!valid_dma(address, RX_BYTES));
        }
        assert_eq!(TX_BYTES % 256, 0);
    }
}
