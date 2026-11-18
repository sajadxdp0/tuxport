//! Guest pointer types (the kernel/mm map, design doc §12 item 2).
//!
//! Addresses in syscall arguments are plain `u64`s or the types here and can be accessed only
//! through [`Mm`](crate::mm::Mm); the kernel layer never dereferences them as host pointers. There
//! is no typed pointer: a syscall that takes a struct pointer copies the struct's bytes in or out
//! and encodes the fields by hand.

/// A range of bytes in a guest address space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserSlice {
    /// Start guest address.
    pub addr: u64,
    /// Length (bytes).
    pub len: usize,
}

impl UserSlice {
    /// Builds a description of a piece of guest memory, without any checks.
    pub fn new(addr: u64, len: usize) -> Self {
        UserSlice { addr, len }
    }
}
