//! A `RefCell` for kernel objects (the kernel/process map): the same as the standard library's,
//! except that debug builds and builds with the `borrow-check` feature also count how many each
//! thread holds borrowed.
//!
//! Kernel objects are protected by the BKL, and they must not still be borrowed when the BKL is
//! released (waiting, [`Cx::unlocked`](crate::kernel::Cx::unlocked)): another thread borrowing the
//! same object at that moment panics (already borrowed), and its process is ended for hitting a
//! kernel bug. The compiler stops borrows obtained through `cx`, but not borrows obtained from an
//! `Rc` in hand (`file.borrow()`), and the temporary borrows of `if let`, `match` and `let … else`
//! are easy to miss. The places that release the BKL call [`assert_none_held`], so every wait
//! checks, and the problem shows up without needing a concurrent collision. Debug builds (cargo
//! test) always check; release builds check when the `borrow-check` feature is on (`tuxport-run`
//! turns it on by default, and it is what the differential tests, LTP and surveys use; the debug
//! app turns it on too). In the app's release build the count does not exist, just like the
//! standard library's `RefCell`.
//!
//! A thread's own `TaskState` (borrowed only by the thread it belongs to, and held borrowed for the
//! whole syscall) and thread-local variables still use the standard library's `RefCell`.

use std::cell;
use std::fmt;
use std::ops::{Deref, DerefMut};

#[cfg(any(debug_assertions, feature = "borrow-check"))]
thread_local! {
    /// How many [`RefCell`]s this thread holds borrowed.
    #[allow(clippy::missing_const_for_thread_local)] // already const; Android clippy misfires
    static BORROWS: cell::Cell<usize> = const { cell::Cell::new(0) };
}

/// One live borrow: in checking builds it adds one when created and subtracts one when dropped.
struct Held;

impl Held {
    #[inline]
    fn new() -> Held {
        #[cfg(any(debug_assertions, feature = "borrow-check"))]
        BORROWS.with(|count| count.set(count.get() + 1));
        Held
    }
}

#[cfg(any(debug_assertions, feature = "borrow-check"))]
impl Drop for Held {
    #[inline]
    fn drop(&mut self) {
        BORROWS.with(|count| count.set(count.get() - 1));
    }
}

/// About to release the BKL: in checking builds, checks that this thread holds no [`RefCell`]
/// borrowed.
#[inline]
pub(crate) fn assert_none_held() {
    #[cfg(any(debug_assertions, feature = "borrow-check"))]
    {
        let n = BORROWS.get();
        assert!(
            n == 0,
            "releasing the BKL while holding {n} RefCell borrow(s)"
        );
    }
}

/// The standard library's `RefCell`, with borrows counted by checking builds (module docs).
#[derive(Default)]
pub(crate) struct RefCell<T: ?Sized>(cell::RefCell<T>);

/// A borrow from [`RefCell::borrow`].
pub(crate) struct Ref<'b, T: ?Sized> {
    inner: cell::Ref<'b, T>,
    _held: Held,
}

/// A borrow from [`RefCell::borrow_mut`].
pub(crate) struct RefMut<'b, T: ?Sized> {
    inner: cell::RefMut<'b, T>,
    _held: Held,
}

impl<T> RefCell<T> {
    pub const fn new(value: T) -> RefCell<T> {
        RefCell(cell::RefCell::new(value))
    }
}

impl<T: ?Sized> RefCell<T> {
    #[inline]
    #[track_caller]
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref {
            inner: self.0.borrow(),
            _held: Held::new(),
        }
    }

    #[inline]
    #[track_caller]
    pub fn borrow_mut(&self) -> RefMut<'_, T> {
        RefMut {
            inner: self.0.borrow_mut(),
            _held: Held::new(),
        }
    }

    pub fn try_borrow(&self) -> Result<Ref<'_, T>, cell::BorrowError> {
        self.0.try_borrow().map(|inner| Ref {
            inner,
            _held: Held::new(),
        })
    }

    pub fn try_borrow_mut(&self) -> Result<RefMut<'_, T>, cell::BorrowMutError> {
        self.0.try_borrow_mut().map(|inner| RefMut {
            inner,
            _held: Held::new(),
        })
    }

    pub fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RefCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: ?Sized> Deref for Ref<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> Deref for RefMut<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> DerefMut for RefMut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}
