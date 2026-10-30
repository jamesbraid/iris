//! Ported from the QEMU support headers (qemu/bswap.h, qemu/bitops.h,
//! qemu/host-utils.h) as hw/display/sgi/ and hw/misc/sgi-odyssey.c use
//! them, in jamesbraid/qemu-sgi-origami.
//!
//! The SGI graphics code below this module is a line-for-line port of C.
//! These are the C idioms it leans on: a nullable, copyable pointer for the
//! places the C keeps one (an opaque callback argument, a plane pointer
//! into a fixed table), QEMU's byte-order loads and stores, and the bit
//! helpers.

use std::ffi::c_void;
use std::ops::{Deref, DerefMut};

/// A C pointer: nullable, copyable, unchecked. Dereferencing a null one
/// panics; the C would have crashed there too.
#[repr(transparent)]
pub struct P<T: ?Sized>(pub *mut T);

impl<T> P<T> {
    pub const fn null() -> Self {
        P(std::ptr::null_mut())
    }
    pub fn new(r: &mut T) -> Self {
        P(r as *mut T)
    }
    pub fn from_ref(r: &T) -> Self {
        P(r as *const T as *mut T)
    }
    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }
    /// The pointee, for as long as the caller says. The C model's own
    /// lifetime rules apply.
    #[allow(clippy::mut_from_ref)]
    pub fn get<'a>(&self) -> &'a mut T {
        assert!(!self.0.is_null(), "null pointer dereference");
        unsafe { &mut *self.0 }
    }
    pub fn as_opt<'a>(&self) -> Option<&'a mut T> {
        if self.0.is_null() { None } else { Some(unsafe { &mut *self.0 }) }
    }
    pub fn cast<U>(&self) -> P<U> {
        P(self.0 as *mut U)
    }
    pub fn opaque(&self) -> Opaque {
        P(self.0 as *mut c_void)
    }
}

impl<T: ?Sized> Clone for P<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: ?Sized> Copy for P<T> {}
impl<T> Default for P<T> {
    fn default() -> Self {
        P::null()
    }
}
impl<T: ?Sized> PartialEq for P<T> {
    fn eq(&self, o: &Self) -> bool {
        std::ptr::addr_eq(self.0, o.0)
    }
}
impl<T: ?Sized> std::fmt::Debug for P<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "P({:p})", self.0)
    }
}
// The C model is single-threaded under its device lock.
unsafe impl<T: ?Sized> Send for P<T> {}
unsafe impl<T: ?Sized> Sync for P<T> {}

impl<T> Deref for P<T> {
    type Target = T;
    fn deref(&self) -> &T {
        assert!(!self.0.is_null(), "null pointer dereference");
        unsafe { &*self.0 }
    }
}
impl<T> DerefMut for P<T> {
    fn deref_mut(&mut self) -> &mut T {
        assert!(!self.0.is_null(), "null pointer dereference");
        unsafe { &mut *self.0 }
    }
}

/// `void *opaque`.
pub type Opaque = P<c_void>;

/// The `opaque` a callback was registered with, back as its type.
pub fn from_opaque<'a, T>(o: Opaque) -> &'a mut T {
    o.cast::<T>().get()
}

// qemu/bswap.h

pub fn ldub_p(b: &[u8]) -> u32 {
    b[0] as u32
}
pub fn lduw_be_p(b: &[u8]) -> u32 {
    u16::from_be_bytes([b[0], b[1]]) as u32
}
pub fn lduw_le_p(b: &[u8]) -> u32 {
    u16::from_le_bytes([b[0], b[1]]) as u32
}
pub fn ldl_be_p(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
pub fn ldl_le_p(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
pub fn ldq_be_p(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}
pub fn ldq_le_p(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}
pub fn stb_p(b: &mut [u8], v: u32) {
    b[0] = v as u8;
}
pub fn stw_be_p(b: &mut [u8], v: u32) {
    b[..2].copy_from_slice(&(v as u16).to_be_bytes());
}
pub fn stw_le_p(b: &mut [u8], v: u32) {
    b[..2].copy_from_slice(&(v as u16).to_le_bytes());
}
pub fn stl_be_p(b: &mut [u8], v: u32) {
    b[..4].copy_from_slice(&v.to_be_bytes());
}
pub fn stl_le_p(b: &mut [u8], v: u32) {
    b[..4].copy_from_slice(&v.to_le_bytes());
}
pub fn stq_be_p(b: &mut [u8], v: u64) {
    b[..8].copy_from_slice(&v.to_be_bytes());
}
pub fn stq_le_p(b: &mut [u8], v: u64) {
    b[..8].copy_from_slice(&v.to_le_bytes());
}

// qemu/bitops.h

pub fn extract32(v: u32, start: u32, len: u32) -> u32 {
    debug_assert!(len > 0 && start + len <= 32);
    (v >> start) & (u32::MAX >> (32 - len))
}
pub fn sextract32(v: u32, start: u32, len: u32) -> i32 {
    debug_assert!(len > 0 && start + len <= 32);
    ((v << (32 - len - start)) as i32) >> (32 - len)
}
pub fn deposit32(v: u32, start: u32, len: u32, field: u32) -> u32 {
    debug_assert!(len > 0 && start + len <= 32);
    let mask = (u32::MAX >> (32 - len)) << start;
    (v & !mask) | ((field << start) & mask)
}
pub fn extract64(v: u64, start: u32, len: u32) -> u64 {
    debug_assert!(len > 0 && start + len <= 64);
    (v >> start) & (u64::MAX >> (64 - len))
}
pub fn sextract64(v: u64, start: u32, len: u32) -> i64 {
    debug_assert!(len > 0 && start + len <= 64);
    ((v << (64 - len - start)) as i64) >> (64 - len)
}
pub fn deposit64(v: u64, start: u32, len: u32, field: u64) -> u64 {
    debug_assert!(len > 0 && start + len <= 64);
    let mask = (u64::MAX >> (64 - len)) << start;
    (v & !mask) | ((field << start) & mask)
}

// qemu/host-utils.h

pub fn ctz32(v: u32) -> u32 {
    v.trailing_zeros()
}
pub fn clz32(v: u32) -> u32 {
    v.leading_zeros()
}
pub fn ctz64(v: u64) -> u32 {
    v.trailing_zeros()
}
pub fn clz64(v: u64) -> u32 {
    v.leading_zeros()
}
pub fn ctpop32(v: u32) -> u32 {
    v.count_ones()
}
pub fn ctpop64(v: u64) -> u32 {
    v.count_ones()
}
pub fn is_power_of_2(v: u64) -> bool {
    v != 0 && v & (v - 1) == 0
}
pub fn pow2ceil(v: u64) -> u64 {
    v.next_power_of_two()
}

/// C's float-to-integer conversion where the value is in range; out of
/// range saturates (where C is undefined).
pub fn f2i(v: f64) -> i32 {
    v as i32
}

/// `qemu_log_mask(LOG_UNIMP | LOG_GUEST_ERROR, ...)`: a guest-triggerable
/// message, on the `odyssey` devlog channel.
#[macro_export]
macro_rules! qlog {
    ($($arg:tt)*) => {
        $crate::dlog!($crate::devlog::LogModule::Odyssey, $($arg)*)
    };
}
