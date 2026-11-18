//! Rust wrapper for the native side (oracle.c / oracle.S).

use std::ffi::c_int;
use std::ptr::NonNull;

use crate::State;

/// Size of the data area: two pages, the only memory access cases may touch.
pub const DATA_BYTES: usize = 8192;

#[repr(C)]
struct RawOracle {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn lockstep_oracle_new() -> *mut RawOracle;
    fn lockstep_oracle_free(oracle: *mut RawOracle);
    fn lockstep_oracle_slot(oracle: *const RawOracle) -> u64;
    fn lockstep_oracle_page(oracle: *const RawOracle) -> *const u8;
    fn lockstep_oracle_data(oracle: *const RawOracle) -> *mut u8;
    fn lockstep_oracle_run(
        oracle: *mut RawOracle,
        insns: *const u32,
        n: u32,
        st: *mut State,
    ) -> c_int;
}

/// A JIT page that can run the tested instructions repeatedly, plus a two-page data area.
pub(crate) struct Oracle {
    raw: NonNull<RawOracle>,
}

impl Oracle {
    /// Allocates the JIT page and data area, and installs the signal handlers.
    pub(crate) fn new() -> Option<Self> {
        NonNull::new(unsafe { lockstep_oracle_new() }).map(|raw| Oracle { raw })
    }

    /// The address of the first tested instruction, i.e. the PC when the hardware runs it.
    pub(crate) fn slot_addr(&self) -> u64 {
        unsafe { lockstep_oracle_slot(self.raw.as_ptr()) }
    }

    /// The contents of the 4K page holding the tested instructions.
    pub(crate) fn slot_page(&self) -> &[u8] {
        let page = unsafe { lockstep_oracle_page(self.raw.as_ptr()) };
        let offset = (self.slot_addr() & !0xfff) - page as u64;
        unsafe { std::slice::from_raw_parts(page.add(offset as usize), 4096) }
    }

    /// The data area's address.
    pub(crate) fn data_addr(&self) -> u64 {
        unsafe { lockstep_oracle_data(self.raw.as_ptr()) as u64 }
    }

    /// The data area's contents.
    pub(crate) fn data(&mut self) -> &mut [u8] {
        let p = unsafe { lockstep_oracle_data(self.raw.as_ptr()) };
        unsafe { std::slice::from_raw_parts_mut(p, DATA_BYTES) }
    }

    /// Runs `insns` on the hardware. Returns `Ok` on success and the signal number when a signal
    /// is raised.
    pub(crate) fn run(&mut self, insns: &[u32], state: &mut State) -> Result<(), i32> {
        let rc = unsafe {
            lockstep_oracle_run(self.raw.as_ptr(), insns.as_ptr(), insns.len() as u32, state)
        };
        match rc {
            0 => Ok(()),
            -1 => panic!("lockstep oracle: bad case or mprotect failed"),
            sig => Err(sig),
        }
    }
}

impl Drop for Oracle {
    fn drop(&mut self) {
        unsafe { lockstep_oracle_free(self.raw.as_ptr()) }
    }
}
