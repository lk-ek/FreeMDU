//! Persistent autonomous scan state. Hardware-independent; host-tested with fake NOR flash.
use embedded_storage::nor_flash::NorFlash;

pub const BASE: u32 = 0x3f0000;
pub const SIZE: u32 = 0x10000;
const RECORD: u32 = 128;
const SECTOR: u32 = 4096;
const SLOTS: u32 = (SIZE - SECTOR) / RECORD; // Last sector backs up the partition table.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Idle,
    Running,
    Paused,
    Found,
    Done,
    StorageError,
}
impl Phase {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Found => "found",
            Self::Done => "done",
            Self::StorageError => "storage_error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub phase: Phase,
    pub software_id: u16,
    pub start: u16,
    pub end: u16,
    pub next: u32,
    pub minimum_ms: u16,
    pub timeout_ms: u16,
    pub maximum_ms: u16,
    pub known_mask: u32,
    pub found: Option<u16>,
    pub errors: u32,
    pub increases: u32,
    pub tested: u32,
}
impl State {
    pub const fn empty() -> Self {
        Self {
            phase: Phase::Idle,
            software_id: 0,
            start: 0,
            end: 0xffff,
            next: 0,
            minimum_ms: 100,
            timeout_ms: 100,
            maximum_ms: 500,
            known_mask: 0,
            found: None,
            errors: 0,
            increases: 0,
            tested: 0,
        }
    }
    pub fn start(id: u16, start: u16, end: u16, minimum: u16, maximum: u16) -> Option<Self> {
        if start > end
            || minimum < 40
            || minimum > maximum
            || maximum > 2000
            || minimum % 5 != 0
            || maximum % 5 != 0
        {
            return None;
        }
        Some(Self {
            phase: Phase::Running,
            software_id: id,
            start,
            end,
            next: u32::from(start),
            minimum_ms: minimum,
            timeout_ms: minimum,
            maximum_ms: maximum,
            ..Self::empty()
        })
    }
    /// Retry all silence observations when their timing assumption changes.
    /// At the cap, pause instead of skipping an untestable key.
    pub fn failure(&mut self) {
        self.errors = self.errors.saturating_add(1);
        if self.timeout_ms < self.maximum_ms {
            self.timeout_ms = self.timeout_ms.saturating_add(5).min(self.maximum_ms);
            self.increases = self.increases.saturating_add(1);
            self.next = u32::from(self.start);
            self.known_mask = 0;
            self.tested = 0;
        } else {
            self.phase = Phase::Paused;
        }
    }
}

fn put(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn get(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn crc(bytes: &[u8]) -> u32 {
    let mut c = !0_u32;
    for byte in bytes {
        c ^= u32::from(*byte);
        for _ in 0..8 {
            c = (c >> 1) ^ (0xedb88320_u32 & (0_u32.wrapping_sub(c & 1)));
        }
    }
    !c
}
fn encode(state: State, sequence: u32) -> [u8; 128] {
    let mut b = [0xff; 128];
    b[..4].copy_from_slice(b"FKS3");
    put(&mut b, 4, sequence);
    b[8] = state.phase as u8;
    for (offset, value) in [
        (12, u32::from(state.software_id)),
        (16, u32::from(state.start)),
        (20, u32::from(state.end)),
        (24, state.next),
        (28, u32::from(state.minimum_ms)),
        (32, u32::from(state.timeout_ms)),
        (36, u32::from(state.maximum_ms)),
        (40, state.known_mask),
        (44, state.found.map_or(0x10000, u32::from)),
        (48, state.errors),
        (52, state.increases),
        (56, state.tested),
    ] {
        put(&mut b, offset, value);
    }
    let sum = crc(&b[..120]);
    put(&mut b, 120, sum);
    b[124..128].copy_from_slice(b"DONE");
    b
}
fn decode(b: &[u8; 128]) -> Option<(u32, State)> {
    if &b[..4] != b"FKS3" || &b[124..] != b"DONE" || get(b, 120) != crc(&b[..120]) {
        return None;
    }
    let phase = match b[8] {
        0 => Phase::Idle,
        1 => Phase::Running,
        2 => Phase::Paused,
        3 => Phase::Found,
        4 => Phase::Done,
        5 => Phase::StorageError,
        _ => return None,
    };
    let mut s = State::start(
        get(b, 12).try_into().ok()?,
        get(b, 16).try_into().ok()?,
        get(b, 20).try_into().ok()?,
        get(b, 28).try_into().ok()?,
        get(b, 36).try_into().ok()?,
    )?;
    s.phase = phase;
    s.next = get(b, 24);
    s.timeout_ms = get(b, 32).try_into().ok()?;
    s.known_mask = get(b, 40);
    s.found = if get(b, 44) == 0x10000 {
        None
    } else {
        Some(get(b, 44).try_into().ok()?)
    };
    s.errors = get(b, 48);
    s.increases = get(b, 52);
    s.tested = get(b, 56);
    if s.next < u32::from(s.start)
        || s.next > u32::from(s.end) + 1
        || s.timeout_ms < s.minimum_ms
        || s.timeout_ms > s.maximum_ms
    {
        return None;
    }
    Some((get(b, 4), s))
}

#[derive(Debug)]
pub enum Error<E> {
    Flash(E),
    Layout,
    UnknownData,
}

pub struct Journal<F> {
    flash: F,
    sequence: u32,
    slot: u32,
}
impl<F: NorFlash> Journal<F> {
    pub fn open(mut flash: F) -> Result<(Self, State), Error<F::Error>> {
        // Permit OTA migration from the supplied original map without rewriting
        // its partition table. Require its exact app layout and a free tail.
        if flash.capacity() < (BASE + SIZE) as usize
            || F::ERASE_SIZE != SECTOR as usize
            || F::WRITE_SIZE > 4
            || 4 % F::WRITE_SIZE != 0
        {
            return Err(Error::Layout);
        }
        let mut app0 = false;
        let mut app1 = false;
        for i in 0..96 {
            let mut entry = [0_u8; 32];
            flash
                .read(0x8000 + i * 32, &mut entry)
                .map_err(Error::Flash)?;
            if entry[..2] == [0xff, 0xff] || entry[..2] == [0xeb, 0xeb] {
                break;
            }
            if entry[..2] != [0xaa, 0x50] {
                return Err(Error::Layout);
            }
            let offset = get(&entry, 4);
            let size = get(&entry, 8);
            let end = offset.checked_add(size).ok_or(Error::Layout)?;
            if entry[2] == 0 {
                match (entry[3], offset, size) {
                    (0x10, 0x10000, 0x1f0000) => app0 = true,
                    (0x11, 0x200000, 0x1f0000) => app1 = true,
                    _ => return Err(Error::Layout),
                }
            }
            if offset < BASE + SIZE
                && end > BASE
                && !(entry[2] == 1
                    && entry[3] == 0x40
                    && offset == BASE
                    && size == SIZE
                    && &entry[12..20] == b"keyscan\0"
                    && get(&entry, 28) == 0)
            {
                return Err(Error::Layout);
            }
        }
        if !app0 || !app1 {
            return Err(Error::Layout);
        }
        let mut latest = None;
        let mut dirty = false;
        let mut recognized = false;
        for slot in 0..SLOTS {
            let mut b = [0; 128];
            flash
                .read(BASE + slot * RECORD, &mut b)
                .map_err(Error::Flash)?;
            dirty |= b.iter().any(|v| *v != 0xff);
            recognized |= &b[..4] == b"FKS3";
            if let Some((seq, s)) = decode(&b) {
                if latest
                    .as_ref()
                    .is_none_or(|(old, _, _): &(u32, u32, State)| {
                        seq.wrapping_sub(*old) < 0x8000_0000 && seq != *old
                    })
                {
                    latest = Some((seq, slot, s));
                }
            }
        }
        // A damaged journal must not resurrect earlier negatives as valid.
        if latest.is_none() && dirty && !recognized {
            return Err(Error::UnknownData);
        }
        let (sequence, slot, state) = latest.map_or((0, 0, State::empty()), |(seq, slot, s)| {
            (seq, (slot + 1) % SLOTS, s)
        });
        Ok((
            Self {
                flash,
                sequence,
                slot,
            },
            state,
        ))
    }
    pub fn save(&mut self, state: State) -> Result<(), Error<F::Error>> {
        // Skip torn records. Erase only on entering a new sector, so the last
        // committed record survives power loss during erase/write/commit.
        loop {
            let address = BASE + self.slot * RECORD;
            if address % SECTOR == 0 {
                self.flash
                    .erase(address, address + SECTOR)
                    .map_err(Error::Flash)?;
                break;
            }
            let mut b = [0; 128];
            self.flash.read(address, &mut b).map_err(Error::Flash)?;
            if b.iter().all(|v| *v == 0xff) {
                break;
            }
            self.slot = (self.slot + 1) % SLOTS;
        }
        let address = BASE + self.slot * RECORD;
        let next_sequence = self.sequence.wrapping_add(1);
        let b = encode(state, next_sequence);
        self.flash.write(address, &b[..124]).map_err(Error::Flash)?;
        self.flash
            .write(address + 124, &b[124..])
            .map_err(Error::Flash)?;
        let mut verify = [0; 128];
        self.flash
            .read(address, &mut verify)
            .map_err(Error::Flash)?;
        if decode(&verify) != Some((next_sequence, state)) {
            return Err(Error::UnknownData);
        }
        self.sequence = next_sequence;
        self.slot = (self.slot + 1) % SLOTS;
        Ok(())
    }
}

/// Add only the scanner partition to the exact original project table.
/// Never moves app/NVS/OTA partitions. Backup is recoverable over USB; the
/// bootloader cannot recover automatically from power loss during table erase.
pub fn install_partition<F: NorFlash>(mut flash: F) -> Result<(), Error<F::Error>> {
    let _ = Journal::open(&mut flash)?; // Bounds, app layout, journal ownership.
    const TARGET: &[u8; 3072] = include_bytes!("../partitions.bin");
    let mut old = [0xff; 4096];
    flash.read(0x8000, &mut old).map_err(Error::Flash)?;
    if old[..3072] == TARGET[..] && old[3072..].iter().all(|b| *b == 0xff) {
        return Ok(());
    }
    if crc(&old[..3072]) != ORIGINAL_CRC
        || old[..160] != TARGET[..160]
        || !old[3072..].iter().all(|b| *b == 0xff)
    {
        return Err(Error::Layout);
    }
    let backup = BASE + SIZE - SECTOR;
    flash.erase(backup, backup + SECTOR).map_err(Error::Flash)?;
    flash.write(backup, &old).map_err(Error::Flash)?;
    let mut buffer = [0xff; 4096];
    flash.read(backup, &mut buffer).map_err(Error::Flash)?;
    if buffer != old {
        return Err(Error::UnknownData);
    }
    buffer.fill(0xff);
    buffer[..3072].copy_from_slice(TARGET);
    let update = (|| {
        flash.erase(0x8000, 0x9000).map_err(Error::Flash)?;
        flash.write(0x8000, &buffer).map_err(Error::Flash)?;
        flash.read(0x8000, &mut buffer).map_err(Error::Flash)?;
        if buffer[..3072] != TARGET[..] || !buffer[3072..].iter().all(|b| *b == 0xff) {
            return Err(Error::UnknownData);
        }
        Ok(())
    })();
    if update.is_err() {
        // Best effort for a reported I/O error. This cannot help after power loss.
        let _ = flash.erase(0x8000, 0x9000);
        let _ = flash.write(0x8000, &old);
    }
    update
}
const ORIGINAL_CRC: u32 = 0xb70e844b;

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_storage::nor_flash::{ErrorType, NorFlashError, NorFlashErrorKind, ReadNorFlash};
    use std::{cell::RefCell, rc::Rc};
    #[derive(Debug, Clone, Copy)]
    struct Fault;
    impl NorFlashError for Fault {
        fn kind(&self) -> NorFlashErrorKind {
            NorFlashErrorKind::Other
        }
    }
    struct Memory {
        bytes: Vec<u8>,
        budget: Option<usize>,
        erased: Vec<u32>,
    }
    #[derive(Clone)]
    struct Flash(Rc<RefCell<Memory>>);
    impl Flash {
        fn new() -> Self {
            let mut bytes = vec![0xff; 0x400000];
            bytes[0x8000..0x8c00]
                .copy_from_slice(include_bytes!("../tests/partitions-original.bin"));
            Self(Rc::new(RefCell::new(Memory {
                bytes,
                budget: None,
                erased: vec![],
            })))
        }
    }
    impl ErrorType for Flash {
        type Error = Fault;
    }
    impl ReadNorFlash for Flash {
        const READ_SIZE: usize = 1;
        fn read(&mut self, offset: u32, b: &mut [u8]) -> Result<(), Fault> {
            b.copy_from_slice(&self.0.borrow().bytes[offset as usize..offset as usize + b.len()]);
            Ok(())
        }
        fn capacity(&self) -> usize {
            self.0.borrow().bytes.len()
        }
    }
    impl NorFlash for Flash {
        const WRITE_SIZE: usize = 4;
        const ERASE_SIZE: usize = 4096;
        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Fault> {
            let mut m = self.0.borrow_mut();
            let n = m.budget.map_or(bytes.len(), |v| v.min(bytes.len()));
            for (i, value) in bytes[..n].iter().enumerate() {
                let p = offset as usize + i;
                assert_eq!(m.bytes[p] & value, *value, "NOR requires erase before 0->1");
                m.bytes[p] &= value;
            }
            if let Some(ref mut b) = m.budget {
                *b -= n;
            }
            if n < bytes.len() { Err(Fault) } else { Ok(()) }
        }
        fn erase(&mut self, from: u32, to: u32) -> Result<(), Fault> {
            assert_eq!(from % 4096, 0);
            assert_eq!(to % 4096, 0);
            let mut m = self.0.borrow_mut();
            m.erased.push(from);
            m.bytes[from as usize..to as usize].fill(0xff);
            Ok(())
        }
    }
    #[test]
    fn cursor_known_keys_and_adapted_timeout_survive_restart() {
        let flash = Flash::new();
        let (mut journal, _) = Journal::open(flash.clone()).unwrap();
        let mut state = State::start(410, 0, 65535, 40, 500).unwrap();
        state.timeout_ms = 65;
        state.next = 12000;
        state.known_mask = 31;
        journal.save(state).unwrap();
        assert_eq!(Journal::open(flash).unwrap().1, state);
    }
    #[test]
    fn partial_record_never_advances_progress() {
        for cut in [0, 1, 4, 80, 120, 123, 124, 125, 127] {
            let flash = Flash::new();
            let (mut journal, _) = Journal::open(flash.clone()).unwrap();
            let first = State::start(410, 0, 65535, 40, 500).unwrap();
            journal.save(first).unwrap();
            let mut next = first;
            next.next = 64;
            flash.0.borrow_mut().budget = Some(cut);
            assert!(journal.save(next).is_err());
            flash.0.borrow_mut().budget = None;
            let (mut recovered, actual) = Journal::open(flash.clone()).unwrap();
            assert_eq!(actual, first, "cut {cut}");
            recovered.save(next).unwrap();
            assert_eq!(Journal::open(flash).unwrap().1, next);
        }
    }
    #[test]
    fn power_loss_after_erasing_next_sector_keeps_previous_checkpoint() {
        let flash = Flash::new();
        let (mut journal, _) = Journal::open(flash.clone()).unwrap();
        let mut state = State::start(410, 0, 65535, 40, 500).unwrap();
        for n in 0..32 {
            state.next = n;
            journal.save(state).unwrap();
        }
        flash.0.borrow_mut().budget = Some(0);
        let mut next = state;
        next.next += 1;
        assert!(journal.save(next).is_err());
        flash.0.borrow_mut().budget = None;
        assert_eq!(Journal::open(flash).unwrap().1, state);
    }

    #[test]
    fn ring_wrap_preserves_latest_and_never_erases_backup() {
        let flash = Flash::new();
        let (mut journal, _) = Journal::open(flash.clone()).unwrap();
        let mut s = State::start(410, 0, 65535, 40, 500).unwrap();
        for next in 0..(SLOTS + 33) {
            s.next = next;
            journal.save(s).unwrap();
        }
        assert_eq!(Journal::open(flash.clone()).unwrap().1, s);
        assert!(
            flash
                .0
                .borrow()
                .erased
                .iter()
                .all(|v| *v >= BASE && *v < BASE + SIZE - SECTOR)
        );
    }
    #[test]
    fn raising_timeout_rechecks_silence_and_cap_pauses() {
        let mut s = State::start(410, 0, 65535, 40, 50).unwrap();
        s.next = 900;
        s.known_mask = 31;
        s.tested = 905;
        s.failure();
        assert_eq!(s.timeout_ms, 45);
        assert_eq!(s.next, 0);
        assert_eq!(s.known_mask, 0);
        assert_eq!(s.tested, 0);
        s.failure();
        assert_eq!(s.timeout_ms, 50);
        s.failure();
        assert_eq!(s.phase, Phase::Paused);
    }
    #[test]
    fn parameters_reject_overflow_and_non_five_ms_steps() {
        for (min, max) in [(35, 500), (41, 500), (40, 503), (100, 50), (40, 2005)] {
            assert!(State::start(410, 0, 65535, min, max).is_none());
        }
        assert!(State::start(410, 10, 9, 40, 500).is_none());
    }
    #[test]
    fn corrupted_crc_is_not_a_checkpoint() {
        let s = State::empty();
        let mut b = encode(s, 1);
        b[24] ^= 1;
        assert!(decode(&b).is_none());
    }
    #[test]
    fn overlapping_partition_is_rejected_without_writes() {
        let flash = Flash::new();
        flash.0.borrow_mut().bytes[0x8004..0x8008].copy_from_slice(&BASE.to_le_bytes());
        assert!(Journal::open(flash.clone()).is_err());
        assert!(flash.0.borrow().erased.is_empty());
    }
    #[test]
    fn migration_is_additive_backed_up_and_idempotent() {
        let flash = Flash::new();
        let original = flash.0.borrow().bytes.clone();
        install_partition(flash.clone()).unwrap();
        let migrated = flash.0.borrow().bytes.clone();
        assert_eq!(&migrated[0x8000..0x80a0], &original[0x8000..0x80a0]);
        assert_eq!(&migrated[0x3ff000..], &original[0x8000..0x9000]);
        assert_eq!(&migrated[0x9000..0x3ff000], &original[0x9000..0x3ff000]);
        assert_eq!(
            &migrated[0x8000..0x8c00],
            include_bytes!("../partitions.bin")
        );
        let erases = flash.0.borrow().erased.len();
        install_partition(flash.clone()).unwrap();
        assert_eq!(flash.0.borrow().erased.len(), erases);
        assert!(Journal::open(flash).is_ok());
    }
    #[test]
    fn backup_failure_does_not_touch_boot_partition_table() {
        let flash = Flash::new();
        let original = flash.0.borrow().bytes[0x8000..0x9000].to_vec();
        flash.0.borrow_mut().budget = Some(100);
        assert!(install_partition(flash.clone()).is_err());
        assert_eq!(&flash.0.borrow().bytes[0x8000..0x9000], &original);
    }
    #[test]
    fn foreign_table_is_not_migrated() {
        let flash = Flash::new();
        flash.0.borrow_mut().bytes[0x800c] = b'X';
        assert!(install_partition(flash.clone()).is_err());
        assert!(flash.0.borrow().erased.is_empty());
    }
}
