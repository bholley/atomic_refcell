//! Implements a container type providing RefCell-like semantics for objects
//! shared across threads.
//!
//! RwLock is traditionally considered to be the |Sync| analogue of RefCell.
//! However, for consumers that can guarantee that they will never mutably
//! borrow the contents concurrently with immutable borrows, an RwLock is
//! overkill, and has key disadvantages:
//! * Performance: Even the fastest existing implementation of RwLock (that of
//!   parking_lot) performs at least two atomic operations during immutable
//!   borrows. This makes mutable borrows significantly cheaper than immutable
//!   borrows, leading to weird incentives when writing performance-critical
//!   code.
//! * Features: Implementing AtomicRefCell on top of RwLock makes it impossible
//!   to implement useful things like AtomicRef{,Mut}::map.
//!
//! As such, we re-implement RefCell semantics from scratch with a single atomic
//! reference count. The primary complication of this scheme relates to keeping
//! things in a consistent state when one thread performs an illegal borrow and
//! panics. Since an AtomicRefCell can be accessed by multiple threads, and since
//! panics are recoverable, we need to ensure that an illegal (panicking) access by
//! one thread does not lead to undefined behavior on other, still-running threads.
//!
//! So we represent things as follows:
//! * Any value with the high bit set (so half the total refcount space) indicates
//!   a mutable borrow.
//! * Mutable borrows perform an atomic compare-and-swap, swapping in the high bit
//!   if the current value is zero. If the current value is non-zero, the thread
//!   panics and the value is left undisturbed.
//! * Immutable borrows perform an atomic increment. If the new value has the high
//!   bit set, the thread panics. The incremented refcount is left as-is, since it
//!   still represents a valid mutable borrow. When the mutable borrow is released,
//!   the refcount is set unconditionally to zero, clearing any stray increments by
//!   panicked threads.
//!
//! There are a few additional purely-academic complications to handle overflow,
//! which are documented in the implementation.
//!
//! The rest of this module is mostly derived by copy-pasting the implementation of
//! RefCell and fixing things up as appropriate. Certain non-threadsafe methods
//! have been removed. We segment the concurrency logic from the rest of the code to
//! keep the tricky parts small and easy to audit.

#![no_std]
#![allow(unsafe_code)]
#![deny(missing_docs)]

use core::cell::UnsafeCell;
use core::cmp;
use core::fmt;
use core::fmt::{Debug, Display};
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use core::sync::atomic;
use core::sync::atomic::AtomicUsize;

#[cfg(feature = "serde")]
extern crate serde;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A threadsafe analogue to RefCell.
pub struct AtomicRefCell<T: ?Sized> {
    borrow: AtomicUsize,
    value: UnsafeCell<T>,
}

/// An error returned by [`AtomicRefCell::try_borrow`](struct.AtomicRefCell.html#method.try_borrow).
pub struct BorrowError {
    _private: (),
}

impl Debug for BorrowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BorrowError").finish()
    }
}

impl Display for BorrowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt("already mutably borrowed", f)
    }
}

/// An error returned by [`AtomicRefCell::try_borrow_mut`](struct.AtomicRefCell.html#method.try_borrow_mut).
pub struct BorrowMutError {
    _private: (),
}

impl Debug for BorrowMutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BorrowMutError").finish()
    }
}

impl Display for BorrowMutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt("already borrowed", f)
    }
}

impl<T> AtomicRefCell<T> {
    /// Creates a new `AtomicRefCell` containing `value`.
    #[inline]
    pub const fn new(value: T) -> AtomicRefCell<T> {
        AtomicRefCell {
            borrow: AtomicUsize::new(0),
            value: UnsafeCell::new(value),
        }
    }

    /// Consumes the `AtomicRefCell`, returning the wrapped value.
    #[inline]
    pub fn into_inner(self) -> T {
        debug_assert!(self.borrow.load(atomic::Ordering::Acquire) == 0);
        self.value.into_inner()
    }
}

impl<T: ?Sized> AtomicRefCell<T> {
    /// Immutably borrows the wrapped value.
    #[inline]
    pub fn borrow(&self) -> AtomicRef<T> {
        match AtomicBorrowRef::try_new(&self.borrow) {
            Ok(borrow) => AtomicRef {
                value: unsafe { NonNull::new_unchecked(self.value.get()) },
                borrow,
            },
            Err(s) => panic!("{}", s),
        }
    }

    /// Attempts to immutably borrow the wrapped value, but instead of panicking
    /// on a failed borrow, returns `Err`.
    #[inline]
    pub fn try_borrow(&self) -> Result<AtomicRef<T>, BorrowError> {
        match AtomicBorrowRef::try_new(&self.borrow) {
            Ok(borrow) => Ok(AtomicRef {
                value: unsafe { NonNull::new_unchecked(self.value.get()) },
                borrow,
            }),
            Err(_) => Err(BorrowError { _private: () }),
        }
    }

    /// Mutably borrows the wrapped value.
    #[inline]
    pub fn borrow_mut(&self) -> AtomicRefMut<T> {
        match AtomicBorrowRefMut::try_new(&self.borrow) {
            Ok(borrow) => AtomicRefMut {
                value: unsafe { NonNull::new_unchecked(self.value.get()) },
                borrow,
                marker: PhantomData,
            },
            Err(s) => panic!("{}", s),
        }
    }

    /// Attempts to mutably borrow the wrapped value, but instead of panicking
    /// on a failed borrow, returns `Err`.
    #[inline]
    pub fn try_borrow_mut(&self) -> Result<AtomicRefMut<T>, BorrowMutError> {
        match AtomicBorrowRefMut::try_new(&self.borrow) {
            Ok(borrow) => Ok(AtomicRefMut {
                value: unsafe { NonNull::new_unchecked(self.value.get()) },
                borrow,
                marker: PhantomData,
            }),
            Err(_) => Err(BorrowMutError { _private: () }),
        }
    }

    /// Returns a raw pointer to the underlying data in this cell.
    ///
    /// External synchronization is needed to avoid data races when dereferencing
    /// the pointer.
    #[inline]
    pub fn as_ptr(&self) -> *mut T {
        self.value.get()
    }

    /// Returns a mutable reference to the wrapped value.
    ///
    /// No runtime checks take place (unless debug assertions are enabled)
    /// because this call borrows `AtomicRefCell` mutably at compile-time.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        debug_assert!(self.borrow.load(atomic::Ordering::Acquire) == 0);
        unsafe { &mut *self.value.get() }
    }
}

//
// Core synchronization logic. Keep this section small and easy to audit.
//

const HIGH_BIT: usize = !(::core::usize::MAX >> 1);
const MAX_FAILED_BORROWS: usize = HIGH_BIT + (HIGH_BIT >> 1);

struct AtomicBorrowRef<'b> {
    borrow: &'b AtomicUsize,
}

impl<'b> AtomicBorrowRef<'b> {
    #[inline]
    fn try_new(borrow: &'b AtomicUsize) -> Result<Self, &'static str> {
        let new = borrow.fetch_add(1, atomic::Ordering::Acquire) + 1;
        if new & HIGH_BIT != 0 {
            // If the new count has the high bit set, that almost certainly
            // means there's an pre-existing mutable borrow. In that case,
            // we simply leave the increment as a benign side-effect and
            // return `Err`. Once the mutable borrow is released, the
            // count will be reset to zero unconditionally.
            //
            // The overflow check here ensures that an unbounded number of
            // immutable borrows during the scope of one mutable borrow
            // will soundly trigger a panic (or abort) rather than UB.
            Self::check_overflow(borrow, new);
            Err("already mutably borrowed")
        } else {
            Ok(AtomicBorrowRef { borrow: borrow })
        }
    }

    #[cold]
    #[inline(never)]
    fn check_overflow(borrow: &'b AtomicUsize, new: usize) {
        if new == HIGH_BIT {
            // We overflowed into the reserved upper half of the refcount
            // space. Before panicking, decrement the refcount to leave things
            // in a consistent immutable-borrow state.
            //
            // This can basically only happen if somebody forget()s AtomicRefs
            // in a tight loop.
            borrow.fetch_sub(1, atomic::Ordering::Release);
            panic!("too many immutable borrows");
        } else if new >= MAX_FAILED_BORROWS {
            // During the mutable borrow, an absurd number of threads have
            // attempted to increment the refcount with immutable borrows.
            // To avoid hypothetically wrapping the refcount, we abort the
            // process once a certain threshold is reached.
            //
            // This requires billions of borrows to fail during the scope of
            // one mutable borrow, and so is very unlikely to happen in a real
            // program.
            //
            // To avoid a potential unsound state after overflowing, we make
            // sure the entire process aborts.
            //
            // Right now, there's no stable way to do that without `std`:
            // https://github.com/rust-lang/rust/issues/67952
            // As a workaround, we cause an abort by making this thread panic
            // during the unwinding of another panic.
            //
            // On platforms where the panic strategy is already 'abort', the
            // ForceAbort object here has no effect, as the program already
            // panics before it is dropped.
            struct ForceAbort;
            impl Drop for ForceAbort {
                fn drop(&mut self) {
                    panic!("Aborting to avoid unsound state of AtomicRefCell");
                }
            }
            let _abort = ForceAbort;
            panic!("Too many failed borrows");
        }
    }
}

impl<'b> Drop for AtomicBorrowRef<'b> {
    #[inline]
    fn drop(&mut self) {
        let old = self.borrow.fetch_sub(1, atomic::Ordering::Release);
        // This assertion is technically incorrect in the case where another
        // thread hits the hypothetical overflow case, since we might observe
        // the refcount before it fixes it up (and panics). But that never will
        // never happen in a real program, and this is a debug_assert! anyway.
        debug_assert!(old & HIGH_BIT == 0);
    }
}

struct AtomicBorrowRefMut<'b> {
    borrow: &'b AtomicUsize,
}

impl<'b> Drop for AtomicBorrowRefMut<'b> {
    #[inline]
    fn drop(&mut self) {
        self.borrow.store(0, atomic::Ordering::Release);
    }
}

impl<'b> AtomicBorrowRefMut<'b> {
    #[inline]
    fn try_new(borrow: &'b AtomicUsize) -> Result<AtomicBorrowRefMut<'b>, &'static str> {
        // Use compare-and-swap to avoid corrupting the immutable borrow count
        // on illegal mutable borrows.
        let old = match borrow.compare_exchange(
            0,
            HIGH_BIT,
            atomic::Ordering::Acquire,
            atomic::Ordering::Relaxed,
        ) {
            Ok(x) => x,
            Err(x) => x,
        };

        if old == 0 {
            Ok(AtomicBorrowRefMut { borrow })
        } else if old & HIGH_BIT == 0 {
            Err("already immutably borrowed")
        } else {
            Err("already mutably borrowed")
        }
    }
}

unsafe impl<T: ?Sized + Send> Send for AtomicRefCell<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for AtomicRefCell<T> {}

//
// End of core synchronization logic. No tricky thread stuff allowed below
// this point.
//

impl<T: Clone> Clone for AtomicRefCell<T> {
    #[inline]
    fn clone(&self) -> AtomicRefCell<T> {
        AtomicRefCell::new(self.borrow().clone())
    }
}

impl<T: Default> Default for AtomicRefCell<T> {
    #[inline]
    fn default() -> AtomicRefCell<T> {
        AtomicRefCell::new(Default::default())
    }
}

impl<T: ?Sized + PartialEq> PartialEq for AtomicRefCell<T> {
    #[inline]
    fn eq(&self, other: &AtomicRefCell<T>) -> bool {
        *self.borrow() == *other.borrow()
    }
}

impl<T: ?Sized + Eq> Eq for AtomicRefCell<T> {}

impl<T: ?Sized + PartialOrd> PartialOrd for AtomicRefCell<T> {
    #[inline]
    fn partial_cmp(&self, other: &AtomicRefCell<T>) -> Option<cmp::Ordering> {
        self.borrow().partial_cmp(&*other.borrow())
    }
}

impl<T: ?Sized + Ord> Ord for AtomicRefCell<T> {
    #[inline]
    fn cmp(&self, other: &AtomicRefCell<T>) -> cmp::Ordering {
        self.borrow().cmp(&*other.borrow())
    }
}

impl<T> From<T> for AtomicRefCell<T> {
    fn from(t: T) -> AtomicRefCell<T> {
        AtomicRefCell::new(t)
    }
}

impl<'b> Clone for AtomicBorrowRef<'b> {
    #[inline]
    fn clone(&self) -> AtomicBorrowRef<'b> {
        AtomicBorrowRef::try_new(self.borrow).unwrap()
    }
}

/// A wrapper type for an immutably borrowed value from an `AtomicRefCell<T>`.
pub struct AtomicRef<'b, T: ?Sized + 'b> {
    value: NonNull<T>,
    borrow: AtomicBorrowRef<'b>,
}

// SAFETY: `AtomicRef<'_, T> acts as a reference. `AtomicBorrowRef` is a
// reference to an atomic.
unsafe impl<'b, T: ?Sized> Sync for AtomicRef<'b, T> where for<'a> &'a T: Sync {}
unsafe impl<'b, T: ?Sized> Send for AtomicRef<'b, T> where for<'a> &'a T: Send {}

impl<'b, T: ?Sized> Deref for AtomicRef<'b, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: We hold shared borrow of the value.
        unsafe { self.value.as_ref() }
    }
}

impl<'b, T: ?Sized> AtomicRef<'b, T> {
    /// Copies an `AtomicRef`.
    #[inline]
    pub fn clone(orig: &AtomicRef<'b, T>) -> AtomicRef<'b, T> {
        AtomicRef {
            value: orig.value,
            borrow: orig.borrow.clone(),
        }
    }

    /// Make a new `AtomicRef` for a component of the borrowed data.
    #[inline]
    pub fn map<U: ?Sized, F>(orig: AtomicRef<'b, T>, f: F) -> AtomicRef<'b, U>
    where
        F: FnOnce(&T) -> &U,
    {
        AtomicRef {
            value: NonNull::from(f(&*orig)),
            borrow: orig.borrow,
        }
    }

    /// Make a new `AtomicRef` for an optional component of the borrowed data.
    #[inline]
    pub fn filter_map<U: ?Sized, F>(orig: AtomicRef<'b, T>, f: F) -> Option<AtomicRef<'b, U>>
    where
        F: FnOnce(&T) -> Option<&U>,
    {
        Some(AtomicRef {
            value: NonNull::from(f(&*orig)?),
            borrow: orig.borrow,
        })
    }
}

impl<'b, T: ?Sized> AtomicRefMut<'b, T> {
    /// Make a new `AtomicRefMut` for a component of the borrowed data, e.g. an enum
    /// variant.
    #[inline]
    pub fn map<U: ?Sized, F>(mut orig: AtomicRefMut<'b, T>, f: F) -> AtomicRefMut<'b, U>
    where
        F: FnOnce(&mut T) -> &mut U,
    {
        AtomicRefMut {
            value: NonNull::from(f(&mut *orig)),
            borrow: orig.borrow,
            marker: PhantomData,
        }
    }

    /// Make a new `AtomicRefMut` for an optional component of the borrowed data.
    #[inline]
    pub fn filter_map<U: ?Sized, F>(
        mut orig: AtomicRefMut<'b, T>,
        f: F,
    ) -> Option<AtomicRefMut<'b, U>>
    where
        F: FnOnce(&mut T) -> Option<&mut U>,
    {
        Some(AtomicRefMut {
            value: NonNull::from(f(&mut *orig)?),
            borrow: orig.borrow,
            marker: PhantomData,
        })
    }
}

/// A wrapper type for a mutably borrowed value from an `AtomicRefCell<T>`.
pub struct AtomicRefMut<'b, T: ?Sized + 'b> {
    value: NonNull<T>,
    borrow: AtomicBorrowRefMut<'b>,
    // `NonNull` is covariant over `T`, but this is used in place of a mutable
    // reference so we need to be invariant over `T`.
    marker: PhantomData<&'b mut T>,
}

// SAFETY: `AtomicRefMut<'_, T> acts as a mutable reference.
// `AtomicBorrowRefMut` is a reference to an atomic.
unsafe impl<'b, T: ?Sized> Sync for AtomicRefMut<'b, T> where for<'a> &'a mut T: Sync {}
unsafe impl<'b, T: ?Sized> Send for AtomicRefMut<'b, T> where for<'a> &'a mut T: Send {}

impl<'b, T: ?Sized> Deref for AtomicRefMut<'b, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: We hold an exclusive borrow of the value.
        unsafe { self.value.as_ref() }
    }
}

impl<'b, T: ?Sized> DerefMut for AtomicRefMut<'b, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: We hold an exclusive borrow of the value.
        unsafe { self.value.as_mut() }
    }
}

impl<'b, T: ?Sized + Debug + 'b> Debug for AtomicRef<'b, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        <T as Debug>::fmt(self, f)
    }
}

impl<'b, T: ?Sized + Debug + 'b> Debug for AtomicRefMut<'b, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        <T as Debug>::fmt(self, f)
    }
}

impl<T: ?Sized + Debug> Debug for AtomicRefCell<T>  {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_borrow() {
            Ok(borrow) => f.debug_struct("AtomicRefCell").field("value", &borrow).finish(),
            Err(_) => {
                // The RefCell is mutably borrowed so we can't look at its value
                // here. Show a placeholder instead.
                struct BorrowedPlaceholder;

                impl Debug for BorrowedPlaceholder {
                    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("<borrowed>")
                    }
                }

                f.debug_struct("AtomicRefCell").field("value", &BorrowedPlaceholder).finish()
            }
        }
    }
}

#[cfg(feature = "serde")]
impl<'de, T: Deserialize<'de>> Deserialize<'de> for AtomicRefCell<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(feature = "serde")]
impl<T: Serialize> Serialize for AtomicRefCell<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::Error;
        match self.try_borrow() {
            Ok(value) => value.serialize(serializer),
            Err(_err) => Err(S::Error::custom("already mutably borrowed")),
        }
    }
}
