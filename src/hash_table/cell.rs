use crate::ebr::{Arc, AtomicArc, Barrier, Ptr, Tag};
use crate::wait_queue::{AsyncWait, WaitQueue};

use std::borrow::Borrow;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::fence;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

/// The fixed size of the main [`DataArray`].
///
/// The size cannot exceed `32`.
pub const CELL_LEN: usize = 32;

/// The fixed size of the linked [`DataArray`].
const LINKED_LEN: usize = CELL_LEN / 4;

/// State bits.
const KILLED: u32 = 1_u32 << 31;
const WAITING: u32 = 1_u32 << 30;
const LOCK: u32 = 1_u32 << 29;
const SLOCK_MAX: u32 = LOCK - 1;
const LOCK_MASK: u32 = LOCK | SLOCK_MAX;

/// [`Cell`] is a small fixed-size hash table that resolves hash conflicts using a linked list
/// of entry arrays.
pub(crate) struct Cell<K: 'static + Eq, V: 'static, const LOCK_FREE: bool> {
    /// An array of key-value pairs and their metadata.
    data_array: DataArray<K, V, CELL_LEN>,

    /// The state of the [`Cell`].
    state: AtomicU32,

    /// The number of valid entries in the [`Cell`].
    num_entries: u32,

    /// The wait queue of the [`Cell`].
    wait_queue: WaitQueue,
}

impl<K: 'static + Eq, V: 'static, const LOCK_FREE: bool> Default for Cell<K, V, LOCK_FREE> {
    fn default() -> Self {
        Cell::<K, V, LOCK_FREE> {
            data_array: DataArray::new(),
            state: AtomicU32::new(0),
            num_entries: 0,
            wait_queue: WaitQueue::default(),
        }
    }
}

impl<K: 'static + Eq, V: 'static, const LOCK_FREE: bool> Cell<K, V, LOCK_FREE> {
    /// Returns true if the [`Cell`] has been killed.
    #[inline]
    pub(crate) fn killed(&self) -> bool {
        (self.state.load(Relaxed) & KILLED) == KILLED
    }

    /// Returns the number of entries in the [`Cell`].
    #[inline]
    pub(crate) fn num_entries(&self) -> usize {
        self.num_entries as usize
    }

    /// Iterates the contents of the [`Cell`].
    #[inline]
    pub(crate) fn iter<'b>(&'b self, barrier: &'b Barrier) -> EntryIterator<'b, K, V, LOCK_FREE> {
        EntryIterator::new(self, barrier)
    }

    /// Searches for an entry associated with the given key.
    #[inline]
    pub(crate) fn search<'b, Q>(
        &'b self,
        key_ref: &Q,
        partial_hash: u8,
        barrier: &'b Barrier,
    ) -> Option<&'b (K, V)>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        if self.num_entries == 0 {
            return None;
        }

        if let Some((_, entry_ref)) = Self::search_array(&self.data_array, key_ref, partial_hash) {
            return Some(entry_ref);
        }

        let mut data_array_ptr = self.data_array.link.load(Acquire, barrier);
        while let Some(data_array_ref) = data_array_ptr.as_ref() {
            if let Some((_, entry_ref)) = Self::search_array(data_array_ref, key_ref, partial_hash)
            {
                return Some(entry_ref);
            }
            data_array_ptr = data_array_ref.link.load(Acquire, barrier);
        }

        None
    }

    /// Gets an [`EntryIterator`] pointing to an entry associated with the given key.
    ///
    /// The returned [`EntryIterator`] always points to a valid entry.
    #[inline]
    pub(crate) fn get<'b, Q>(
        &'b self,
        key_ref: &Q,
        partial_hash: u8,
        barrier: &'b Barrier,
    ) -> Option<EntryIterator<'b, K, V, LOCK_FREE>>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        if self.num_entries == 0 {
            return None;
        }

        if let Some((index, _)) = Self::search_array(&self.data_array, key_ref, partial_hash) {
            return Some(EntryIterator {
                cell: Some(self),
                current_array_ptr: Ptr::null(),
                prev_array_ptr: Ptr::null(),
                current_index: index,
                barrier_ref: barrier,
            });
        }

        let mut current_array_ptr = self.data_array.link.load(Acquire, barrier);
        let mut prev_array_ptr = Ptr::null();
        while let Some(data_array_ref) = current_array_ptr.as_ref() {
            if let Some((index, _)) = Self::search_array(data_array_ref, key_ref, partial_hash) {
                return Some(EntryIterator {
                    cell: Some(self),
                    current_array_ptr,
                    prev_array_ptr,
                    current_index: index,
                    barrier_ref: barrier,
                });
            }
            prev_array_ptr = current_array_ptr;
            current_array_ptr = data_array_ref.link.load(Acquire, barrier);
        }

        None
    }

    /// Kills the [`Cell`] for dropping it.
    #[inline]
    pub(crate) unsafe fn kill_and_drop(&self, barrier: &Barrier) {
        if !self.data_array.link.load(Acquire, barrier).is_null() {
            if let Some(data_array) = self.data_array.link.swap((None, Tag::None), Relaxed).0 {
                barrier.reclaim(data_array);
            }
        }
        self.state.store(KILLED, Relaxed);
        ptr::read(self);
    }

    /// Searches the given [`DataArray`] for an entry matching the key.
    #[inline]
    fn search_array<'b, Q, const LEN: usize>(
        data_array_ref: &'b DataArray<K, V, LEN>,
        key_ref: &Q,
        partial_hash: u8,
    ) -> Option<(usize, &'b (K, V))>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        let mut occupied = if LOCK_FREE {
            data_array_ref.occupied & (!data_array_ref.removed)
        } else {
            data_array_ref.occupied
        };
        if LOCK_FREE {
            fence(Acquire);
        }

        // Look into the preferred slot.
        let preferred_index = partial_hash as usize % LEN;
        if (occupied & (1_u32 << preferred_index)) != 0
            && data_array_ref.partial_hash_array[preferred_index] == partial_hash
        {
            let entry_ptr = data_array_ref.data[preferred_index].as_ptr();
            let entry_ref = unsafe { &(*entry_ptr) };
            if entry_ref.0.borrow() == key_ref {
                return Some((preferred_index, entry_ref));
            }
            occupied &= !(1_u32 << preferred_index);
        }

        // Look into other slots.
        let mut current_index = occupied.trailing_zeros();
        while (current_index as usize) < LEN {
            if data_array_ref.partial_hash_array[current_index as usize] == partial_hash {
                let entry_ptr = data_array_ref.data[current_index as usize].as_ptr();
                let entry_ref = unsafe { &(*entry_ptr) };
                if entry_ref.0.borrow() == key_ref {
                    return Some((current_index as usize, entry_ref));
                }
            }
            occupied &= !(1_u32 << current_index);
            current_index = occupied.trailing_zeros();
        }

        None
    }

    /// Searches for a next closest valid slot to the given slot in the [`DataArray`].
    ///
    /// If the given slot is valid, it returns the given slot.
    fn next_entry<Q, const LEN: usize>(
        data_array_ref: &DataArray<K, V, LEN>,
        current_index: usize,
    ) -> Option<(usize, &(K, V), u8)>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        if current_index >= CELL_LEN {
            return None;
        }

        let occupied = if LOCK_FREE {
            (data_array_ref.occupied & (!data_array_ref.removed))
                & (!((1_u32 << current_index) - 1))
        } else {
            data_array_ref.occupied & (!((1_u32 << current_index) - 1))
        };

        if LOCK_FREE {
            fence(Acquire);
        }

        let next_index = occupied.trailing_zeros() as usize;
        if next_index < CELL_LEN {
            let entry_ptr = data_array_ref.data[next_index].as_ptr();
            return Some((
                next_index,
                unsafe { &(*entry_ptr) },
                data_array_ref.partial_hash_array[next_index],
            ));
        }

        None
    }
}

impl<K: 'static + Eq, V: 'static, const LOCK_FREE: bool> Drop for Cell<K, V, LOCK_FREE> {
    fn drop(&mut self) {
        // The [`Cell`] must have been killed.
        debug_assert!(self.killed());
    }
}

pub struct EntryIterator<'b, K: 'static + Eq, V: 'static, const LOCK_FREE: bool> {
    cell: Option<&'b Cell<K, V, LOCK_FREE>>,
    current_array_ptr: Ptr<'b, DataArray<K, V, LINKED_LEN>>,
    prev_array_ptr: Ptr<'b, DataArray<K, V, LINKED_LEN>>,
    current_index: usize,
    barrier_ref: &'b Barrier,
}

impl<'b, K: 'static + Eq, V: 'static, const LOCK_FREE: bool> EntryIterator<'b, K, V, LOCK_FREE> {
    /// Creates a new [`EntryIterator`].
    #[inline]
    pub(crate) fn new(
        cell: &'b Cell<K, V, LOCK_FREE>,
        barrier: &'b Barrier,
    ) -> EntryIterator<'b, K, V, LOCK_FREE> {
        EntryIterator {
            cell: Some(cell),
            current_array_ptr: Ptr::null(),
            prev_array_ptr: Ptr::null(),
            current_index: usize::MAX,
            barrier_ref: barrier,
        }
    }

    /// Gets a reference to the key-value pair.
    #[inline]
    pub(crate) fn get(&self) -> &'b (K, V) {
        let entry_ptr = if let Some(data_array_ref) = self.current_array_ptr.as_ref() {
            data_array_ref.data[self.current_index].as_ptr()
        } else {
            self.cell.as_ref().unwrap().data_array.data[self.current_index].as_ptr()
        };
        unsafe { &(*entry_ptr) }
    }

    /// Tries to remove the current data array from the linked list.
    ///
    /// It should only be invoked when the caller is holding a [`Locker`] on the [`Cell`].
    fn unlink_data_array(&mut self, data_array_ref: &DataArray<K, V, LINKED_LEN>) {
        let next_data_array = if LOCK_FREE {
            data_array_ref.link.get_arc(Relaxed, self.barrier_ref)
        } else {
            data_array_ref.link.swap((None, Tag::None), Relaxed).0
        };
        self.current_array_ptr = next_data_array
            .as_ref()
            .map_or_else(Ptr::null, |n| n.ptr(self.barrier_ref));
        let old_data_array = if let Some(prev_data_array_ref) = self.prev_array_ptr.as_ref() {
            prev_data_array_ref
                .link
                .swap((next_data_array, Tag::None), Relaxed)
                .0
        } else if let Some(cell) = self.cell.as_ref() {
            cell.data_array
                .link
                .swap((next_data_array, Tag::None), Relaxed)
                .0
        } else {
            None
        };
        if let Some(data_array) = old_data_array {
            self.barrier_ref.reclaim(data_array);
        }
        if self.current_array_ptr.is_null() {
            self.cell.take();
        } else {
            self.current_index = usize::MAX;
        }
    }
}

impl<'b, K: 'static + Eq, V: 'static, const LOCK_FREE: bool> EntryIterator<'b, K, V, LOCK_FREE> {
    fn next_entry<const LEN: usize>(
        &mut self,
        data_array_ref: &'b DataArray<K, V, LEN>,
    ) -> Option<(&'b (K, V), u8)> {
        // Search for the next valid entry.
        let current_index = if self.current_index == usize::MAX {
            0
        } else {
            self.current_index + 1
        };
        if let Some((index, entry_ref, hash)) =
            Cell::<K, V, LOCK_FREE>::next_entry(data_array_ref, current_index)
        {
            self.current_index = index;
            return Some((entry_ref, hash));
        }

        self.current_array_ptr = data_array_ref.link.load(Acquire, self.barrier_ref);
        self.current_index = usize::MAX;

        None
    }
}

impl<'b, K: 'static + Eq, V: 'static, const LOCK_FREE: bool> Iterator
    for EntryIterator<'b, K, V, LOCK_FREE>
{
    type Item = (&'b (K, V), u8);
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(&cell) = self.cell.as_ref() {
            if self.current_array_ptr.is_null() {
                if let Some(result) = self.next_entry(&cell.data_array) {
                    return Some(result);
                }
            }
            while let Some(data_array_ref) = self.current_array_ptr.as_ref() {
                if let Some(result) = self.next_entry(data_array_ref) {
                    return Some(result);
                }
            }
            // Fuse itself.
            self.cell.take();
        }
        None
    }
}

pub struct Locker<'b, K: 'static + Eq, V: 'static, const LOCK_FREE: bool> {
    cell: &'b Cell<K, V, LOCK_FREE>,
}

impl<'b, K: Eq, V, const LOCK_FREE: bool> Locker<'b, K, V, LOCK_FREE> {
    /// Locks the [`Cell`].
    #[inline]
    pub(crate) fn lock(
        cell: &'b Cell<K, V, LOCK_FREE>,
        barrier: &'b Barrier,
    ) -> Option<Locker<'b, K, V, LOCK_FREE>> {
        loop {
            if let Ok(locker) = Self::try_lock(cell, barrier) {
                return locker;
            }
            if let Ok(locker) = cell.wait_queue.wait_sync(|| {
                // Mark that there is a waiting thread.
                cell.state.fetch_or(WAITING, Release);
                Self::try_lock(cell, barrier)
            }) {
                return locker;
            }
        }
    }

    /// Tries to lock the [`Cell`], and if it fails, pushes an [`AsyncWait`].
    #[inline]
    pub(crate) fn try_lock_or_wait(
        cell: &'b Cell<K, V, LOCK_FREE>,
        async_wait: *mut AsyncWait,
        barrier: &'b Barrier,
    ) -> Result<Option<Locker<'b, K, V, LOCK_FREE>>, ()> {
        if let Ok(locker) = Self::try_lock(cell, barrier) {
            return Ok(locker);
        }
        cell.wait_queue.push_async_entry(async_wait, || {
            // Mark that there is a waiting thread.
            cell.state.fetch_or(WAITING, Release);
            Self::try_lock(cell, barrier)
        })
    }

    /// Returns a reference to the [`Cell`].
    #[inline]
    pub(crate) fn cell(&self) -> &'b Cell<K, V, LOCK_FREE> {
        self.cell
    }

    /// Inserts a new key-value pair into the [`Cell`] without a uniqueness check.
    #[inline]
    pub(crate) fn insert(&'b self, key: K, value: V, partial_hash: u8, barrier: &'b Barrier) {
        assert!(self.cell.num_entries != u32::MAX, "array overflow");

        let preferred_index = partial_hash as usize % CELL_LEN;
        if (self.cell.data_array.occupied & (1_u32 << preferred_index)) == 0 {
            self.insert_entry(
                &mut self.cell_mut().data_array,
                preferred_index,
                key,
                value,
                partial_hash,
            );
            return;
        }
        let free_index = self.cell.data_array.occupied.trailing_ones() as usize;
        if free_index < CELL_LEN {
            self.insert_entry(
                &mut self.cell_mut().data_array,
                free_index,
                key,
                value,
                partial_hash,
            );
            return;
        }

        let preferred_index = partial_hash as usize % LINKED_LEN;
        let mut data_array_ptr = self.cell.data_array.link.load(Acquire, barrier).as_raw()
            as *mut DataArray<K, V, LINKED_LEN>;
        while let Some(data_array_mut) = unsafe { data_array_ptr.as_mut() } {
            if (data_array_mut.occupied & (1_u32 << preferred_index)) == 0 {
                self.insert_entry(data_array_mut, preferred_index, key, value, partial_hash);
                return;
            }
            let free_index = data_array_mut.occupied.trailing_ones() as usize;
            if free_index < LINKED_LEN {
                self.insert_entry(data_array_mut, free_index, key, value, partial_hash);
                return;
            }

            data_array_ptr = data_array_mut.link.load(Acquire, barrier).as_raw()
                as *mut DataArray<K, V, LINKED_LEN>;
        }

        // Insert a new `DataArray` at the linked list head.
        let mut new_data_array = Arc::new(DataArray::new());
        self.insert_entry(
            unsafe { new_data_array.get_mut().unwrap() },
            preferred_index,
            key,
            value,
            partial_hash,
        );
        new_data_array.link.swap(
            (
                self.cell.data_array.link.get_arc(Relaxed, barrier),
                Tag::None,
            ),
            Relaxed,
        );
        self.cell
            .data_array
            .link
            .swap((Some(new_data_array), Tag::None), Release);
    }

    /// Removes a key-value pair being pointed by the given [`EntryIterator`].
    #[inline]
    pub(crate) fn erase(&self, iterator: &mut EntryIterator<K, V, LOCK_FREE>) -> Option<(K, V)> {
        if iterator.current_index == usize::MAX {
            return None;
        }

        if iterator.current_array_ptr.is_null() {
            self.erase_entry(&mut self.cell_mut().data_array, iterator.current_index)
        } else {
            let data_array_mut = unsafe {
                &mut *(iterator.current_array_ptr.as_raw() as *mut DataArray<K, V, LINKED_LEN>)
            };
            let result = self.erase_entry(data_array_mut, iterator.current_index);
            if LOCK_FREE && (data_array_mut.occupied & (!data_array_mut.removed)) == 0
                || (!LOCK_FREE && data_array_mut.occupied == 0)
            {
                iterator.unlink_data_array(data_array_mut);
            }
            result
        }
    }

    /// Extracts the key-value pair being pointed by `self`.
    #[inline]
    pub(crate) fn extract(&self, iterator: &mut EntryIterator<K, V, LOCK_FREE>) -> (K, V) {
        debug_assert!(!LOCK_FREE);
        if iterator.current_array_ptr.is_null() {
            self.extract_entry(&mut self.cell_mut().data_array, iterator.current_index)
        } else {
            let data_array_mut = unsafe {
                &mut *(iterator.current_array_ptr.as_raw() as *mut DataArray<K, V, LINKED_LEN>)
            };
            let extracted = self.extract_entry(data_array_mut, iterator.current_index);
            if data_array_mut.occupied == 0 {
                iterator.unlink_data_array(data_array_mut);
            }
            extracted
        }
    }

    /// Purges all the data.
    #[inline]
    pub(crate) fn purge(&mut self, barrier: &Barrier) {
        if LOCK_FREE {
            self.cell_mut().data_array.removed = self.cell.data_array.occupied;
        }
        self.cell.state.fetch_or(KILLED, Release);
        self.num_entries_updated(0);
        if !self.cell.data_array.link.load(Acquire, barrier).is_null() {
            if let Some(data_array) = self.cell.data_array.link.swap((None, Tag::None), Relaxed).0 {
                barrier.reclaim(data_array);
            }
        }
    }

    /// Removes a key-value pair in the slot.
    fn insert_entry<const LEN: usize>(
        &self,
        data_array_mut: &mut DataArray<K, V, LEN>,
        index: usize,
        key: K,
        value: V,
        partial_hash: u8,
    ) {
        debug_assert!(index < LEN);

        unsafe {
            data_array_mut.data[index].as_mut_ptr().write((key, value));
            data_array_mut.partial_hash_array[index] = partial_hash;

            if LOCK_FREE {
                fence(Release);
            }

            data_array_mut.occupied |= 1_u32 << index;
        }
        self.num_entries_updated(self.cell.num_entries + 1);
    }

    /// Removes a key-value pair in the slot.
    fn erase_entry<const LEN: usize>(
        &self,
        data_array_mut: &mut DataArray<K, V, LEN>,
        index: usize,
    ) -> Option<(K, V)> {
        debug_assert!(index < LEN);

        if data_array_mut.occupied & (1_u32 << index) == 0 {
            return None;
        }

        if LOCK_FREE && (data_array_mut.removed & (1_u32 << index)) != 0 {
            return None;
        }

        self.num_entries_updated(self.cell.num_entries - 1);
        if LOCK_FREE {
            data_array_mut.removed |= 1_u32 << index;
            None
        } else {
            data_array_mut.occupied &= !(1_u32 << index);
            let entry_ptr = data_array_mut.data[index].as_mut_ptr();
            #[allow(clippy::uninit_assumed_init)]
            Some(unsafe { ptr::replace(entry_ptr, MaybeUninit::uninit().assume_init()) })
        }
    }

    /// Extracts and removes the key-value pair in the slot.
    fn extract_entry<const LEN: usize>(
        &self,
        data_array_mut: &mut DataArray<K, V, LEN>,
        index: usize,
    ) -> (K, V) {
        debug_assert!(index < LEN);

        self.num_entries_updated(self.cell.num_entries - 1);
        data_array_mut.occupied &= !(1_u32 << index);
        let entry_ptr = data_array_mut.data[index].as_mut_ptr();
        unsafe { ptr::read(entry_ptr) }
    }

    /// Updates the number of entries.
    fn num_entries_updated(&self, num: u32) {
        self.cell_mut().num_entries = num;
    }

    /// Returns a mutable reference to the `Cell`.
    #[allow(clippy::mut_from_ref)]
    fn cell_mut(&self) -> &mut Cell<K, V, LOCK_FREE> {
        #[allow(clippy::cast_ref_to_mut)]
        unsafe {
            &mut *(self.cell as *const _ as *mut Cell<K, V, LOCK_FREE>)
        }
    }

    /// Tries to lock the [`Cell`].
    fn try_lock(
        cell: &'b Cell<K, V, LOCK_FREE>,
        _barrier: &'b Barrier,
    ) -> Result<Option<Locker<'b, K, V, LOCK_FREE>>, ()> {
        let current = cell.state.load(Relaxed) & (!LOCK_MASK);
        if (current & KILLED) == KILLED {
            return Ok(None);
        }
        if cell
            .state
            .compare_exchange(current, current | LOCK, Acquire, Relaxed)
            .is_ok()
        {
            Ok(Some(Locker { cell }))
        } else {
            Err(())
        }
    }
}

impl<'b, K: 'static + Eq, V: 'static, const LOCK_FREE: bool> Drop for Locker<'b, K, V, LOCK_FREE> {
    #[inline]
    fn drop(&mut self) {
        let mut current = self.cell.state.load(Relaxed);
        loop {
            let wakeup = (current & WAITING) == WAITING;
            match self.cell.state.compare_exchange(
                current,
                current & (!(WAITING | LOCK)),
                Release,
                Relaxed,
            ) {
                Ok(_) => {
                    if wakeup {
                        self.cell.wait_queue.signal();
                    }
                    break;
                }
                Err(result) => current = result,
            }
        }
    }
}

pub struct Reader<'b, K: 'static + Eq, V: 'static, const LOCK_FREE: bool> {
    cell: &'b Cell<K, V, LOCK_FREE>,
}

impl<'b, K: Eq, V, const LOCK_FREE: bool> Reader<'b, K, V, LOCK_FREE> {
    /// Locks the given [`Cell`].
    #[inline]
    pub(crate) fn lock(
        cell: &'b Cell<K, V, LOCK_FREE>,
        barrier: &'b Barrier,
    ) -> Option<Reader<'b, K, V, LOCK_FREE>> {
        loop {
            if let Ok(reader) = Self::try_lock(cell, barrier) {
                return reader;
            }
            if let Ok(reader) = cell.wait_queue.wait_sync(|| {
                // Mark that there is a waiting thread.
                cell.state.fetch_or(WAITING, Release);
                Self::try_lock(cell, barrier)
            }) {
                return reader;
            }
        }
    }

    /// Tries to lock the [`Cell`], and if it fails, pushes an [`AsyncWait`].
    #[inline]
    pub(crate) fn try_lock_or_wait(
        cell: &'b Cell<K, V, LOCK_FREE>,
        async_wait: *mut AsyncWait,
        barrier: &'b Barrier,
    ) -> Result<Option<Reader<'b, K, V, LOCK_FREE>>, ()> {
        if let Ok(reader) = Self::try_lock(cell, barrier) {
            return Ok(reader);
        }
        cell.wait_queue.push_async_entry(async_wait, || {
            // Mark that there is a waiting thread.
            cell.state.fetch_or(WAITING, Release);
            Self::try_lock(cell, barrier)
        })
    }

    /// Returns a reference to the [`Cell`].
    #[inline]
    pub(crate) fn cell(&self) -> &'b Cell<K, V, LOCK_FREE> {
        self.cell
    }

    /// Tries to lock the [`Cell`].
    fn try_lock(
        cell: &'b Cell<K, V, LOCK_FREE>,
        _barrier: &'b Barrier,
    ) -> Result<Option<Reader<'b, K, V, LOCK_FREE>>, ()> {
        let current = cell.state.load(Relaxed);
        if (current & LOCK_MASK) >= SLOCK_MAX {
            return Err(());
        }
        if (current & KILLED) >= KILLED {
            return Ok(None);
        }
        if cell
            .state
            .compare_exchange(current, current + 1, Acquire, Relaxed)
            .is_ok()
        {
            Ok(Some(Reader { cell }))
        } else {
            Err(())
        }
    }
}

impl<'b, K: 'static + Eq, V: 'static, const LOCK_FREE: bool> Drop for Reader<'b, K, V, LOCK_FREE> {
    #[inline]
    fn drop(&mut self) {
        let mut current = self.cell.state.load(Relaxed);
        loop {
            let wakeup = (current & WAITING) == WAITING;
            let next = (current - 1) & !(WAITING);
            match self
                .cell
                .state
                .compare_exchange(current, next, Relaxed, Relaxed)
            {
                Ok(_) => {
                    if wakeup {
                        self.cell.wait_queue.signal();
                    }
                    break;
                }
                Err(result) => current = result,
            }
        }
    }
}

/// [`DataArray`] is a fixed size array of key-value pairs.
pub struct DataArray<K: 'static + Eq, V: 'static, const LEN: usize> {
    link: AtomicArc<DataArray<K, V, LINKED_LEN>>,
    occupied: u32,
    removed: u32,
    partial_hash_array: [u8; LEN],
    data: [MaybeUninit<(K, V)>; LEN],
}

impl<K: 'static + Eq, V: 'static, const LEN: usize> DataArray<K, V, LEN> {
    fn new() -> DataArray<K, V, LEN> {
        DataArray {
            link: AtomicArc::null(),
            occupied: 0,
            removed: 0,
            partial_hash_array: [0_u8; LEN],
            data: unsafe { MaybeUninit::uninit().assume_init() },
        }
    }
}

impl<K: 'static + Eq, V: 'static, const LEN: usize> Drop for DataArray<K, V, LEN> {
    fn drop(&mut self) {
        let mut occupied = self.occupied;
        let mut index = occupied.trailing_zeros();
        while (index as usize) < LEN {
            let entry_mut_ptr = self.data[index as usize].as_mut_ptr();
            unsafe {
                ptr::drop_in_place(entry_mut_ptr);
            }
            occupied &= !(1_u32 << index);
            index = occupied.trailing_zeros();
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::convert::TryInto;
    use std::sync::atomic::AtomicPtr;

    use tokio::sync;

    #[tokio::test(flavor = "multi_thread", worker_threads = 16)]
    async fn queue() {
        let num_tasks = CELL_LEN + 2;
        let barrier = Arc::new(sync::Barrier::new(num_tasks));
        let cell: Arc<Cell<usize, usize, true>> = Arc::new(Cell::default());
        let mut data: [u64; 128] = [0; 128];
        let mut task_handles = Vec::with_capacity(num_tasks);
        for task_id in 0..num_tasks {
            let barrier_copied = barrier.clone();
            let cell_copied = cell.clone();
            let data_ptr = AtomicPtr::new(&mut data);
            task_handles.push(tokio::spawn(async move {
                barrier_copied.wait().await;
                let barrier = Barrier::new();
                for i in 0..2048 {
                    let exclusive_locker = Locker::lock(&*cell_copied, &barrier).unwrap();
                    let mut sum: u64 = 0;
                    for j in 0..128 {
                        unsafe {
                            sum += (*data_ptr.load(Relaxed))[j];
                            (*data_ptr.load(Relaxed))[j] = if i % 4 == 0 { 2 } else { 4 }
                        };
                    }
                    assert_eq!(sum % 256, 0);
                    if i == 0 {
                        exclusive_locker.insert(
                            task_id,
                            0,
                            (task_id % CELL_LEN).try_into().unwrap(),
                            &barrier,
                        );
                    } else {
                        assert_eq!(
                            exclusive_locker
                                .cell()
                                .search(
                                    &task_id,
                                    (task_id % CELL_LEN).try_into().unwrap(),
                                    &barrier
                                )
                                .unwrap(),
                            &(task_id, 0_usize)
                        );
                    }
                    drop(exclusive_locker);

                    let read_locker = Reader::lock(&*cell_copied, &barrier).unwrap();
                    assert_eq!(
                        read_locker
                            .cell()
                            .search(&task_id, (task_id % CELL_LEN).try_into().unwrap(), &barrier)
                            .unwrap(),
                        &(task_id, 0_usize)
                    );
                }
            }));
        }
        for r in futures::future::join_all(task_handles).await {
            assert!(r.is_ok());
        }

        let sum: u64 = data.iter().sum();
        assert_eq!(sum % 256, 0);
        assert_eq!(cell.num_entries(), num_tasks);

        let epoch_barrier = Barrier::new();
        for task_id in 0..num_tasks {
            assert_eq!(
                cell.search(
                    &task_id,
                    (task_id % CELL_LEN).try_into().unwrap(),
                    &epoch_barrier
                ),
                Some(&(task_id, 0))
            );
        }
        let mut iterated = 0;
        for entry in cell.iter(&epoch_barrier) {
            assert!(entry.0 .0 < num_tasks);
            assert_eq!(entry.0 .1, 0);
            iterated += 1;
        }
        assert_eq!(cell.num_entries(), iterated);

        let mut xlocker = Locker::lock(&*cell, &epoch_barrier).unwrap();
        xlocker.purge(&epoch_barrier);
        drop(xlocker);

        assert!(cell.killed());
        assert_eq!(cell.num_entries(), 0);
        assert!(Locker::lock(&*cell, &epoch_barrier).is_none());
    }
}
