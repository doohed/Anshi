//! Shared kernel state. Anshi runs on one CPU and never touches a
//! subsystem's state from an interrupt handler while the same subsystem
//! is mid-update, so nothing here locks.

use core::cell::UnsafeCell;

/// A global the kernel mutates in place: a `static` with interior
/// mutability, for a subsystem's own state.
pub struct Global<T>(UnsafeCell<T>);

// One CPU, no preemption inside the kernel (see the module comment).
unsafe impl<T> Sync for Global<T> {}

impl<T> Global<T> {
    pub const fn new(value: T) -> Global<T> {
        Global(UnsafeCell::new(value))
    }

    /// Where the state lives -- for handing its address to hardware.
    pub const fn as_ptr(&self) -> *mut T {
        self.0.get()
    }

    /// The state itself. Callers must not let two of these overlap -- in
    /// practice, don't hold one across a call back into the same module.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}
