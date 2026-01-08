use std::{
    array,
    cell::UnsafeCell,
    hint::spin_loop,
    marker::PhantomData,
    ops::Deref,
    sync::atomic::{AtomicUsize, Ordering},
    thread::yield_now,
};

#[derive(Debug)]
/// Shared resources for a ring buffer shareable between threads.
pub struct AtomicBuffer<T: Send + Sync, const N: usize, const READERS: usize> {
    data: Box<[UnsafeCell<T>; N]>,
    write_ptr: AtomicUsize,
    read_ptrs: [AtomicUsize; READERS],
}

#[derive(Debug)]
pub struct AtomicBufferWriter<'a, T: Send + Sync, const N: usize, const READERS: usize> {
    data: &'a [UnsafeCell<T>; N],
    write_ptr: &'a AtomicUsize,
    read_ptrs: &'a [AtomicUsize; READERS],
}

#[derive(Debug)]
pub struct AtomicBufferReader<'a, T: Send + Sync, const N: usize> {
    data: &'a [UnsafeCell<T>; N],
    write_ptr: &'a AtomicUsize,
    read_ptr: &'a AtomicUsize,
}

/// Provides a safe handle to the buffered value.
///
/// Advances the read pointer on [`Drop`]. If this type is forgotten,
/// the next handle will return the same value.
#[derive(Debug)]
pub struct AtomicBufferReadHandle<'a, T: Send + Sync, const N: usize> {
    pub value: &'a T,
    read_ptr: &'a AtomicUsize,
    _data_len: PhantomData<[(); N]>,
}

#[derive(Debug)]
pub struct AtomicBufferSplit<'a, T: Send + Sync, const N: usize, const READERS: usize> {
    pub write_ptr: AtomicBufferWriter<'a, T, N, READERS>,
    pub read_ptrs: [AtomicBufferReader<'a, T, N>; READERS],
}

// ---------- Override UnsafeCell Sync ---------- //
// SAFETY: all of these types have write/read behavior protected by the ring
// logic.

unsafe impl<T: Send + Sync, const N: usize, const READERS: usize> Send
    for AtomicBuffer<T, N, READERS>
{
}

unsafe impl<T: Send + Sync, const N: usize, const READERS: usize> Sync
    for AtomicBuffer<T, N, READERS>
{
}

unsafe impl<T: Send + Sync, const N: usize, const READERS: usize> Send
    for AtomicBufferWriter<'_, T, N, READERS>
{
}

unsafe impl<T: Send + Sync, const N: usize, const READERS: usize> Sync
    for AtomicBufferWriter<'_, T, N, READERS>
{
}

unsafe impl<T: Send + Sync, const N: usize> Send for AtomicBufferReader<'_, T, N> {}
unsafe impl<T: Send + Sync, const N: usize> Sync for AtomicBufferReader<'_, T, N> {}

unsafe impl<T: Send + Sync, const N: usize> Send for AtomicBufferReadHandle<'_, T, N> {}
unsafe impl<T: Send + Sync, const N: usize> Sync for AtomicBufferReadHandle<'_, T, N> {}

// ---------- ---------- //

impl<T, const N: usize, const READERS: usize> AtomicBuffer<T, N, READERS>
where
    T: Send + Sync + Default,
{
    /// Creates a ring buffer shareable between threads.
    ///
    /// Has one writer and a static number of readers.
    /// [`Self::split`] must be used to get writers and readers.
    pub fn new() -> Self {
        Self {
            data: Box::new(array::from_fn(|_idx| UnsafeCell::new(T::default()))),
            write_ptr: 0.into(),
            read_ptrs: [0; READERS].map(AtomicUsize::from),
        }
    }
}

impl<T, const N: usize, const READERS: usize> AtomicBuffer<T, N, READERS>
where
    T: Send + Sync,
{
    pub fn split(&mut self) -> AtomicBufferSplit<'_, T, N, READERS> {
        AtomicBufferSplit {
            write_ptr: AtomicBufferWriter {
                data: &self.data,
                write_ptr: &self.write_ptr,
                read_ptrs: &self.read_ptrs,
            },
            read_ptrs: self
                .read_ptrs
                .each_ref()
                .map(|read_ptr| AtomicBufferReader {
                    data: &self.data,
                    write_ptr: &self.write_ptr,
                    read_ptr,
                }),
        }
    }
}

impl<T, const N: usize, const READERS: usize> AtomicBufferWriter<'_, T, N, READERS>
where
    T: Send + Sync,
{
    fn write_inner<U>(&mut self, value: U, write_pos: usize, next_write_pos: usize) -> bool
    where
        T: From<U>,
    {
        // Ring implementation drops an index for simple comparison.
        // Write would only be invalidating reads if the next write idx overlaps.
        if self
            .read_ptrs
            .iter()
            .any(|read_ptr| read_ptr.load(Ordering::Relaxed) == next_write_pos)
        {
            false
        } else {
            let next_item_ptr = &self.data[write_pos];

            // SAFETY: ring buffer logic means this is not read until after
            // the value is fully written.
            let next_item = unsafe { &mut *next_item_ptr.get() };
            *next_item = value.into();

            // The release ordering is coupled with a load ordering in other
            // threads that guarantee next_item is valid.
            self.write_ptr.store(next_write_pos, Ordering::Release);

            true
        }
    }
    /// Will write to the next index if there is capacity.
    ///
    /// Returns true if a write succeeded. Returns false if the buffer is full.
    pub fn try_write<U>(&mut self, value: U) -> bool
    where
        T: From<U>,
    {
        let write_pos = self.write_ptr.load(Ordering::Relaxed);
        let next_write_pos = write_pos.wrapping_add(1) % N;
        self.write_inner(value, write_pos, next_write_pos)
    }

    /// Will write to the next index, spinning until there is capacity.
    pub fn write_spin<U>(&mut self, value: &U)
    where
        T: for<'a> From<&'a U>,
        U: ?Sized,
    {
        let write_pos = self.write_ptr.load(Ordering::Relaxed);
        let next_write_pos = write_pos.wrapping_add(1) % N;
        while !self.write_inner(value, write_pos, next_write_pos) {
            spin_loop();
            yield_now();
        }
    }
}

impl<T, const N: usize> AtomicBufferReader<'_, T, N>
where
    T: Send + Sync,
{
    /// SAFETY: The read pointer must not equal the write pointer.
    ///
    /// The write pointer must also have been loaded with [`Ordering::Acquire`]
    /// to sync the underlying data.
    unsafe fn read_inner(&mut self, read_pos: usize) -> AtomicBufferReadHandle<'_, T, N> {
        let value_ptr = &self.data[read_pos];

        // SAFETY: prior checks ensured that the writer is not mutating
        // this value. It will be kept valid until at least the handle is
        // dropped. Any simultaneous readers cannot modify this data.
        let value = unsafe { &*value_ptr.get() };

        AtomicBufferReadHandle {
            value,
            read_ptr: self.read_ptr,
            _data_len: PhantomData,
        }
    }

    /// Will return the next buffered value if available.
    pub fn try_read(&mut self) -> Option<AtomicBufferReadHandle<'_, T, N>> {
        let read_pos = self.read_ptr.load(Ordering::Relaxed);
        // Equality check also synchronizes buffer memory.
        if read_pos != self.write_ptr.load(Ordering::Acquire) {
            // SAFETY: write_ptr != read_ptr and was acquired.
            Some(unsafe { self.read_inner(read_pos) })
        } else {
            None
        }
    }

    /// Will return the next buffered value, spinning until available.
    pub fn read_spin(&mut self) -> AtomicBufferReadHandle<'_, T, N> {
        let read_pos = self.read_ptr.load(Ordering::Relaxed);

        // In the case that the write pointer hasn't advanced, this saves on
        // expensive memory synchronization.
        while read_pos == self.write_ptr.load(Ordering::Relaxed) {
            spin_loop();
            yield_now();
        }

        // Synchronizes the buffer memory.
        assert_ne!(read_pos, self.write_ptr.load(Ordering::Acquire));

        // SAFETY: write_ptr != read_ptr and was acquired.
        unsafe { self.read_inner(read_pos) }
    }
}

impl<T, const N: usize> Deref for AtomicBufferReadHandle<'_, T, N>
where
    T: Send + Sync,
{
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.value
    }
}

impl<T, const N: usize> AsRef<T> for AtomicBufferReadHandle<'_, T, N>
where
    T: Send + Sync,
{
    fn as_ref(&self) -> &T {
        self.value
    }
}

impl<T, const N: usize> Drop for AtomicBufferReadHandle<'_, T, N>
where
    T: Send + Sync,
{
    fn drop(&mut self) {
        // This will not race, since there is only one handle for a read
        // pointer at a time. No other code changes the value of a read pointer.
        let ptr_val = self.read_ptr.load(Ordering::Relaxed);
        self.read_ptr
            .store(ptr_val.wrapping_add(1) % N, Ordering::Relaxed);
    }
}
