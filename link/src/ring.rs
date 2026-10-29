//! One thread puts, one other thread takes, and they share no lock. Each
//! counter has one writer: the putter writes `put` and `drop_before`, the
//! taker writes `taken`. A lock between threads of different rank can hang a
//! PSP, and this needs none. The counters count forever and wrap, which a
//! length that is a power of two keeps in step with the slots.

use core::marker::PhantomData;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

pub struct Ring<T> {
    slots: AtomicPtr<T>,
    len: AtomicUsize,
    /// Items ever put.
    put: AtomicU32,
    /// Items ever taken or skipped.
    taken: AtomicU32,
    /// Nothing put before this is taken: a flush, without the putter writing
    /// the taker's counter.
    drop_before: AtomicU32,
    _items: PhantomData<T>,
}

impl<T: Copy + Send> Ring<T> {
    pub const fn new() -> Ring<T> {
        Ring {
            slots: AtomicPtr::new(ptr::null_mut()),
            len: AtomicUsize::new(0),
            put: AtomicU32::new(0),
            taken: AtomicU32::new(0),
            drop_before: AtomicU32::new(0),
            _items: PhantomData,
        }
    }

    /// Gives the ring its storage, once, before either side uses it. Its
    /// length is a power of two below 2^31.
    pub fn install(&self, slots: &'static mut [T]) {
        assert!(slots.len().is_power_of_two() && slots.len() < 1 << 31, "a ring's length is a power of two below 2^31");
        let first = self.slots.compare_exchange(ptr::null_mut(), slots.as_mut_ptr(), Ordering::AcqRel, Ordering::Acquire);
        assert!(first.is_ok(), "a ring is given its storage once");
        self.len.store(slots.len(), Ordering::Release);
    }

    /// Copies as much of `items` as fits, oldest first, and says how many.
    /// Never waits, and never writes over what is not taken yet.
    ///
    /// # Safety
    ///
    /// Only the ring's one putting thread calls `put` and `drop_queued`.
    pub unsafe fn put(&self, items: &[T]) -> usize {
        let Some((slots, mask)) = self.storage() else { return 0 };
        let put = self.put.load(Ordering::Relaxed);
        let taken = self.taken.load(Ordering::Acquire);
        self.retire_passed_drop(taken);
        let count = (mask + 1 - put.wrapping_sub(taken) as usize).min(items.len());
        for (offset, &item) in items[..count].iter().enumerate() {
            unsafe { slots.add(put.wrapping_add(offset as u32) as usize & mask).write(item) };
        }
        self.put.store(put.wrapping_add(count as u32), Ordering::Release);
        count
    }

    /// Everything queued now is never taken. What is put after this is.
    ///
    /// # Safety
    ///
    /// As for `put`.
    pub unsafe fn drop_queued(&self) {
        self.drop_before.store(self.put.load(Ordering::Relaxed), Ordering::Release);
    }

    /// Up to `into.len()` items, oldest first, and how many.
    ///
    /// # Safety
    ///
    /// Only the ring's one taking thread calls `take` and `skip_all`.
    pub unsafe fn take(&self, into: &mut [T]) -> usize {
        let Some((slots, mask)) = self.storage() else { return 0 };
        let first = self.first();
        let count = (self.put.load(Ordering::Acquire).wrapping_sub(first) as usize).min(into.len());
        for (offset, item) in into[..count].iter_mut().enumerate() {
            *item = unsafe { slots.add(first.wrapping_add(offset as u32) as usize & mask).read() };
        }
        self.taken.store(first.wrapping_add(count as u32), Ordering::Release);
        count
    }

    /// Skips everything queued.
    ///
    /// # Safety
    ///
    /// As for `take`.
    pub unsafe fn skip_all(&self) {
        self.taken.store(self.put.load(Ordering::Acquire), Ordering::Release);
    }

    /// How many items are queued, as far as the caller's side can tell.
    pub fn queued(&self) -> usize {
        let first = self.first();
        self.put.load(Ordering::Acquire).wrapping_sub(first) as usize
    }

    /// The oldest item still to take. Read before `put`, which only grows,
    /// so it is never past it.
    fn first(&self) -> u32 {
        let skip = self.drop_before.load(Ordering::Acquire);
        let taken = self.taken.load(Ordering::Acquire);
        if (skip.wrapping_sub(taken) as i32) > 0 {
            skip
        } else {
            taken
        }
    }

    /// Moves a drop the taker has passed up to it, so a counter wrap never revives it.
    fn retire_passed_drop(&self, taken: u32) {
        if (self.drop_before.load(Ordering::Relaxed).wrapping_sub(taken) as i32) < 0 {
            self.drop_before.store(taken, Ordering::Release);
        }
    }

    fn storage(&self) -> Option<(*mut T, usize)> {
        let len = self.len.load(Ordering::Acquire);
        (len != 0).then(|| (self.slots.load(Ordering::Acquire), len - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{boxed::Box, thread, vec, vec::Vec};

    /// A ring whose counters start at `start`, so a test can cross the wrap.
    fn ring<T: Copy + Send + 'static>(len: usize, start: u32, fill: T) -> &'static Ring<T> {
        let ring = Box::leak(Box::new(Ring {
            slots: AtomicPtr::new(ptr::null_mut()),
            len: AtomicUsize::new(0),
            put: AtomicU32::new(start),
            taken: AtomicU32::new(start),
            drop_before: AtomicU32::new(start),
            _items: PhantomData,
        }));
        ring.install(vec![fill; len].leak());
        ring
    }

    #[test]
    fn a_million_items_cross_threads_in_order_through_the_wrap() {
        const ITEMS: u32 = 1_000_000;
        let ring = ring(16, u32::MAX - ITEMS / 2, 0u32);
        let putter = thread::spawn(move || {
            let mut next = 0;
            while next < ITEMS {
                let batch: Vec<u32> = (next..ITEMS.min(next + 7)).collect();
                let sent = unsafe { ring.put(&batch) };
                next += sent as u32;
                if sent == 0 {
                    thread::yield_now();
                }
            }
        });
        let (mut expected, mut into) = (0, [0u32; 5]);
        while expected < ITEMS {
            let got = unsafe { ring.take(&mut into) };
            for &item in &into[..got] {
                assert_eq!(item, expected, "every item once, in order");
                expected += 1;
            }
            if got == 0 {
                thread::yield_now();
            }
        }
        putter.join().unwrap();
        assert_eq!(ring.queued(), 0);
        assert!(ring.put.load(Ordering::Relaxed) < ITEMS, "the counters wrapped");
    }

    #[test]
    fn drops_while_both_threads_run_only_ever_skip_forward() {
        const LAST: u32 = 200_000;
        let ring = ring(64, u32::MAX - 20_000, 0u32);
        let putter = thread::spawn(move || {
            let mut next = 1;
            while next <= LAST {
                let sent = unsafe { ring.put(&[next]) };
                next += sent as u32;
                if next % 1000 == 0 {
                    unsafe { ring.drop_queued() };
                }
                if sent == 0 {
                    thread::yield_now();
                }
            }
        });
        let (mut last, mut into) = (0, [0u32; 16]);
        while last < LAST {
            let got = unsafe { ring.take(&mut into) };
            for &item in &into[..got] {
                assert!(item > last, "{item} after {last}");
                last = item;
            }
            if got == 0 {
                thread::yield_now();
            }
        }
        putter.join().unwrap();
    }

    #[test]
    fn put_never_writes_over_what_is_not_taken() {
        let ring = ring(4, u32::MAX - 1, 0u8);
        let mut two = [0; 2];
        let mut all = [0; 8];
        unsafe {
            assert_eq!(ring.put(&[1, 2, 3]), 3);
            assert_eq!(ring.put(&[4, 5, 6]), 1, "one place left");
            assert_eq!(ring.queued(), 4);
            assert_eq!(ring.take(&mut two), 2);
            assert_eq!(ring.put(&[5, 6, 7]), 2);
            assert_eq!(ring.take(&mut all), 4);
        }
        assert_eq!((two, &all[..4]), ([1, 2], &[3, 4, 5, 6][..]));
    }

    #[test]
    fn a_drop_skips_exactly_what_was_queued_when_it_was_asked() {
        let ring = ring(8, u32::MAX - 3, 0u8);
        let mut one = [0; 1];
        let mut all = [0; 8];
        unsafe {
            ring.put(&[1, 2, 3]);
            ring.take(&mut one);
            ring.drop_queued();
            assert_eq!(ring.queued(), 0, "2 and 3 are dropped");
            ring.put(&[7, 8]);
            assert_eq!(ring.queued(), 2, "what is put after the drop is kept");
            assert_eq!(ring.take(&mut all), 2);
            assert_eq!(ring.put(&[0; 8]), 8, "the dropped places are free again");
        }
        assert_eq!((one, &all[..2]), ([1], &[7, 8][..]));
    }

    #[test]
    fn skip_all_empties_and_a_ring_without_storage_holds_nothing() {
        let ring = ring(4, 0, 0u8);
        unsafe {
            ring.put(&[1, 2, 3]);
            ring.skip_all();
            assert_eq!(ring.take(&mut [0; 4]), 0);
        }
        assert_eq!(ring.queued(), 0);
        let bare: Ring<u8> = Ring::new();
        assert_eq!(unsafe { bare.put(&[1]) }, 0);
        assert_eq!(bare.queued(), 0);
    }
}
