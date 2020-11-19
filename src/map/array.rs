use super::cell::{Cell, CellLocker, ARRAY_SIZE};
use crossbeam_epoch::{Atomic, Guard, Shared};
use std::convert::TryInto;
use std::mem::MaybeUninit;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

pub struct Array<K: Eq, V> {
    metadata_array: Vec<Cell<K, V>>,
    entry_array: Vec<MaybeUninit<(K, V)>>,
    lb_capacity: u8,
    rehashing: AtomicUsize,
    rehashed: AtomicUsize,
    old_array: Atomic<Array<K, V>>,
}

impl<K: Eq, V> Array<K, V> {
    pub fn new(capacity: usize, old_array: Atomic<Array<K, V>>) -> Array<K, V> {
        let lb_capacity = Self::calculate_lb_metadata_array_size(capacity);
        let mut array = Array {
            metadata_array: Vec::with_capacity(1usize << lb_capacity),
            entry_array: Vec::with_capacity((1usize << lb_capacity) * (ARRAY_SIZE as usize)),
            lb_capacity: lb_capacity,
            rehashing: AtomicUsize::new(0),
            rehashed: AtomicUsize::new(0),
            old_array: old_array,
        };
        for _ in 0..(1usize << lb_capacity) {
            array.metadata_array.push(Default::default());
        }
        array
    }

    pub fn get_cell(&self, index: usize) -> &Cell<K, V> {
        &self.metadata_array[index]
    }

    pub fn get_entry(&self, index: usize) -> *const (K, V) {
        unsafe { &(*self.entry_array.as_ptr().add(index)) }.as_ptr()
    }

    pub fn num_cells(&self) -> usize {
        1usize << self.lb_capacity
    }

    pub fn capacity(&self) -> usize {
        (1usize << self.lb_capacity) * (ARRAY_SIZE as usize)
    }

    pub fn get_old_array<'a>(&self, guard: &'a Guard) -> Shared<'a, Array<K, V>> {
        self.old_array.load(Relaxed, &guard)
    }

    pub fn calculate_metadata_array_index(&self, hash: u64) -> usize {
        (hash >> (64 - self.lb_capacity)).try_into().unwrap()
    }

    pub fn calculate_lb_metadata_array_size(capacity: usize) -> u8 {
        let adjusted_capacity = capacity.min((usize::MAX / 2) - (ARRAY_SIZE as usize - 1));
        let required_cells = ((adjusted_capacity + (ARRAY_SIZE as usize - 1))
            / (ARRAY_SIZE as usize))
            .next_power_of_two();
        let lb_capacity =
            ((std::mem::size_of::<usize>() * 8) - (required_cells.leading_zeros() as usize) - 1)
                .max(1);

        // 2^lb_capacity * ARRAY_SIZE >= capacity
        debug_assert!(lb_capacity > 0);
        debug_assert!(lb_capacity < (std::mem::size_of::<usize>() * 8));
        debug_assert!((1usize << lb_capacity) * (ARRAY_SIZE as usize) >= adjusted_capacity);
        lb_capacity.try_into().unwrap()
    }

    pub fn kill_cell<F: Fn(&K) -> (u64, u16)>(
        &self,
        cell_locker: &mut CellLocker<K, V>,
        old_array: &Array<K, V>,
        old_cell_index: usize,
        hasher: &F,
    ) {
        if cell_locker.killed() {
            return;
        } else if cell_locker.empty() {
            cell_locker.kill();
            return;
        }

        let shrink = old_array.lb_capacity > self.lb_capacity;
        let ratio = if shrink {
            1usize << (old_array.lb_capacity - self.lb_capacity)
        } else {
            1usize << (self.lb_capacity - old_array.lb_capacity)
        };
        let target_cell_index = if shrink {
            old_cell_index / ratio
        } else {
            old_cell_index * ratio
        };
        let num_target_cells = if shrink { 1 } else { ratio };
        let mut target_cells: Vec<CellLocker<K, V>> = Vec::with_capacity(num_target_cells);

        let mut current = cell_locker.next_occupied(u8::MAX);
        while current != u8::MAX {
            let old_index = old_cell_index * ARRAY_SIZE as usize + current as usize;
            let entry_ptr = unsafe { &(*old_array.entry_array.as_ptr().add(old_index)) }.as_ptr();
            let entry_mut_ptr = entry_ptr as *mut MaybeUninit<(K, V)>;
            let entry = unsafe { std::ptr::replace(entry_mut_ptr, MaybeUninit::uninit()) };
            let (key, value) = unsafe { entry.assume_init() };
            let (hash, partial_hash) = hasher(&key);
            let new_cell_index = self.calculate_metadata_array_index(hash);

            debug_assert!(
                (!shrink && (new_cell_index - target_cell_index) < ratio)
                    || (shrink && new_cell_index == old_cell_index / ratio)
            );

            while target_cells.len() <= (new_cell_index - target_cell_index) {
                let cell_index = target_cell_index + target_cells.len();
                target_cells.push(CellLocker::lock(self.get_cell(cell_index)));
            }

            self.insert(
                key,
                partial_hash,
                value,
                new_cell_index,
                &mut target_cells[new_cell_index - target_cell_index],
            );
            cell_locker.remove(current);
            current = cell_locker.next_occupied(current);
        }

        if cell_locker.overflowing() {
            while let Some((key, value)) = cell_locker.consume_link() {
                let (hash, partial_hash) = hasher(&key);
                let new_cell_index = self.calculate_metadata_array_index(hash);

                debug_assert!(
                    (!shrink && (new_cell_index - target_cell_index) < ratio)
                        || (shrink && new_cell_index == old_cell_index / ratio)
                );
                while target_cells.len() <= (new_cell_index - target_cell_index) {
                    let cell_index = target_cell_index + target_cells.len();
                    target_cells.push(CellLocker::lock(self.get_cell(cell_index)));
                }

                self.insert(
                    key,
                    partial_hash,
                    value,
                    new_cell_index,
                    &mut target_cells[new_cell_index - target_cell_index],
                );
            }
        }

        cell_locker.kill();
    }

    fn insert(
        &self,
        key: K,
        partial_hash: u16,
        value: V,
        cell_index: usize,
        cell_locker: &mut CellLocker<K, V>,
    ) {
        let mut new_sub_index = u8::MAX;
        if let Some(sub_index) = cell_locker.insert(partial_hash) {
            new_sub_index = sub_index;
        }
        if new_sub_index != u8::MAX {
            let entry_array_index = cell_index * (ARRAY_SIZE as usize) + (new_sub_index as usize);
            let entry_mut_ptr = self.get_entry(entry_array_index) as *mut (K, V);
            unsafe { entry_mut_ptr.write((key, value)) };
        } else {
            cell_locker.insert_link(key, partial_hash, value);
        }
    }

    pub fn partial_rehash<F: Fn(&K) -> (u64, u16)>(&self, guard: &Guard, hasher: F) -> bool {
        let old_array = self.old_array.load(Relaxed, guard);
        if old_array.is_null() {
            return true;
        }

        let old_array_ref = unsafe { old_array.deref() };
        let old_array_size = old_array_ref.num_cells();
        let mut current = self.rehashing.load(Relaxed);
        loop {
            if current >= old_array_size {
                return false;
            }
            match self.rehashing.compare_exchange(
                current,
                current + ARRAY_SIZE as usize,
                Acquire,
                Relaxed,
            ) {
                Ok(_) => break,
                Err(result) => current = result,
            }
        }

        for old_cell_index in current..(current + ARRAY_SIZE as usize).min(old_array_size) {
            if old_array_ref.metadata_array[old_cell_index].killed() {
                continue;
            }
            let mut old_cell = CellLocker::lock(&old_array_ref.metadata_array[old_cell_index]);
            self.kill_cell(&mut old_cell, old_array_ref, old_cell_index, &hasher);
        }

        let completed = self.rehashed.fetch_add(ARRAY_SIZE as usize, Release) + ARRAY_SIZE as usize;
        if old_array_size <= completed {
            let old_array = self.old_array.swap(Shared::null(), Relaxed, guard);
            if !old_array.is_null() {
                unsafe { guard.defer_destroy(old_array) };
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn static_assertions() {
        assert_eq!(0usize.next_power_of_two(), 1);
        assert_eq!(1usize.next_power_of_two(), 1);
        assert_eq!(2usize.next_power_of_two(), 2);
        assert_eq!(3usize.next_power_of_two(), 4);
        assert_eq!(1 << 0, 1);
        assert_eq!(0usize.is_power_of_two(), false);
        assert_eq!(1usize.is_power_of_two(), true);
        assert_eq!(19usize / (ARRAY_SIZE as usize), 1);
        for capacity in 0..1024 as usize {
            assert!(
                (1usize << Array::<bool, bool>::calculate_lb_metadata_array_size(capacity))
                    * (ARRAY_SIZE as usize)
                    >= capacity
            );
        }
        assert!(
            (1usize << Array::<bool, bool>::calculate_lb_metadata_array_size(usize::MAX))
                * (ARRAY_SIZE as usize)
                >= (usize::MAX / 2)
        );
        for i in 2..(std::mem::size_of::<usize>() - 3) {
            let capacity = (1usize << i) * (ARRAY_SIZE as usize);
            assert_eq!(
                Array::<bool, bool>::calculate_lb_metadata_array_size(capacity) as usize,
                i
            );
        }
    }
}
