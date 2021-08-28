use super::arc::Underlying;
use super::link::Link;

use std::ptr;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::Ordering::Relaxed;

/// [`AtomicArc`] owns the underlying instance, and allows users to perform atomic operations
/// on it.
pub struct AtomicArc<T> {
    underlying: AtomicPtr<Underlying<T>>,
}

impl<T> AtomicArc<T> {
    pub fn new(t: T) -> AtomicArc<T> {
        let boxed = Box::new(Underlying::new(t));
        AtomicArc {
            underlying: AtomicPtr::new(Box::into_raw(boxed)),
        }
    }
}

impl<T> Drop for AtomicArc<T> {
    fn drop(&mut self) {
        let underlying_ptr = self.underlying.swap(ptr::null_mut(), Relaxed);
        // TODO: push it into a reclaimer.
        unsafe {
            underlying_ptr.as_mut().map(|u| {
                if u.drop_ref() {
                    u.dealloc()
                }
            })
        };
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU8};

    #[test]
    fn arc() {
        static DESTROYED: AtomicBool = AtomicBool::new(false);
        struct A(AtomicU8, usize, &'static AtomicBool);
        impl Drop for A {
            fn drop(&mut self) {
                self.2.swap(true, Relaxed);
            }
        }
        let atomic_arc = AtomicArc::new(A(AtomicU8::new(10), 10, &DESTROYED));
        assert!(!DESTROYED.load(Relaxed));

        drop(atomic_arc);
        assert!(DESTROYED.load(Relaxed));
    }
}
