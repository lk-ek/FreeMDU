//! Serialize OTA and scanner access without constructing a second FlashStorage.
use core::cell::RefCell;
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embedded_storage::{
    ReadStorage, Storage,
    nor_flash::{ErrorType, NorFlash, ReadNorFlash},
};
use esp_storage::{FlashStorage, FlashStorageError};
pub type FlashMutex = Mutex<CriticalSectionRawMutex, RefCell<FlashStorage<'static>>>;
#[derive(Clone, Copy)]
pub struct SharedFlash(pub &'static FlashMutex);
impl ReadStorage for SharedFlash {
    type Error = FlashStorageError;
    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.0
            .lock(|f| ReadStorage::read(&mut *f.borrow_mut(), offset, bytes))
    }
    fn capacity(&self) -> usize {
        self.0.lock(|f| ReadStorage::capacity(&*f.borrow()))
    }
}
impl Storage for SharedFlash {
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0
            .lock(|f| Storage::write(&mut *f.borrow_mut(), offset, bytes))
    }
}
impl ErrorType for SharedFlash {
    type Error = FlashStorageError;
}
impl ReadNorFlash for SharedFlash {
    const READ_SIZE: usize = <FlashStorage<'static> as ReadNorFlash>::READ_SIZE;
    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.0
            .lock(|f| ReadNorFlash::read(&mut *f.borrow_mut(), offset, bytes))
    }
    fn capacity(&self) -> usize {
        ReadStorage::capacity(self)
    }
}
impl NorFlash for SharedFlash {
    const WRITE_SIZE: usize = <FlashStorage<'static> as NorFlash>::WRITE_SIZE;
    const ERASE_SIZE: usize = <FlashStorage<'static> as NorFlash>::ERASE_SIZE;
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0
            .lock(|f| NorFlash::write(&mut *f.borrow_mut(), offset, bytes))
    }
    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.0
            .lock(|f| NorFlash::erase(&mut *f.borrow_mut(), from, to))
    }
}
