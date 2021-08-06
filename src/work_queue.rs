use std::{
    cell::{Cell,UnsafeCell},
    marker::PhantomData,
    mem::{size_of, size_of_val, replace},
    ptr::null_mut,
};

use super::*;

#[repr(C)]
struct Block {
    /// next block in block-chain ;)
    next: *mut Block,

    /// size of block including this header
    size: usize,

    /// offset from beginning of block (&self) to next work-item to be executed
    beg: usize,

    /// offset to end of allocated data in block, self.end <= self.size
    end: usize,
}

#[repr(C)]
struct ItemHeader {
    size: usize,
    zero: usize,
    exec: unsafe fn(*mut u8),
    drop: unsafe fn(*mut u8),
}

/// trait for types that can track work queue statistics
pub trait WorkQueueStats {
    /// single work item enqueued
    fn enqueue(&self) {}

    /// single work item executed
    fn execute(&self) {}

    /// new queue block allocated
    fn block_allocate(&self) {}

    /// block write pointer move to new block
    fn write_rollover(&self) {}

    /// block read pointer moved to new block
    fn read_rollover(&self) {}
}

/// A default work queue statistics type that records nothing and takes no space
#[derive(Default)]
pub struct NopWorkQueueStats;

impl WorkQueueStats for NopWorkQueueStats {}

/// An advanced version of [WorkQueue] with statistics gathering support.
///
/// ```
/// use std::cell::RefCell;
/// use single_threaded_work_queue::{AdvancedWorkQueue,NopWorkQueueStats};
///
/// let value = RefCell::new(0);
///
/// {
///     let mut queue = AdvancedWorkQueue::<NopWorkQueueStats>::new();
///
///     queue.push(|| *value.borrow_mut() += 1);
///     queue.push(|| *value.borrow_mut() += 1);
///
///     queue.pump();
/// }
///
/// assert_eq!(*value.borrow(), 2);
/// ```
pub struct AdvancedWorkQueue<'a, S: WorkQueueStats> {
    first_: UnsafeCell<*mut Block>,
    next_: UnsafeCell<*mut Block>,
    last_: UnsafeCell<*mut Block>,
    pumping: Cell<bool>,
    stats: S,
    _store: PhantomData<*mut dyn WorkItem<'a>>,
}

impl<'a, S: WorkQueueStats + Default> Default for AdvancedWorkQueue<'a, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, S: WorkQueueStats + Default> AdvancedWorkQueue<'a, S> {
    /// create a new work queue without a buffer
    pub fn new() -> Self {
        Self::with_stats(S::default())
    }

    /// create a new work queue with an initial storage capacity
    pub fn with_capacity(size: usize) -> Self {
        Self::with_stats_and_capacity(S::default(), size)
    }
}

impl<'a, S: WorkQueueStats+Sized> AdvancedWorkQueue<'a, S> {

    /// create a new work queue with the specified stats tracker and without a buffer
    pub fn with_stats(stats: S) -> Self {
        Self::internal_construct(stats, null_mut())
    }

    /// create a new work queue with the specified stats tracker and an initial
    /// storage capacity
    pub fn with_stats_and_capacity(stats: S, size: usize) -> Self {
        Self::internal_construct(stats, Self::new_block(size))
    }

    fn internal_construct(stats: S, block: *mut Block) -> Self {
        Self {
            first_: UnsafeCell::new(block),
            next_: UnsafeCell::new(block),
            last_: UnsafeCell::new(block),
            pumping: Cell::new(false),
            stats,
            _store: PhantomData,
        }
    }

    pub fn is_empty(&self) -> bool {
        if self.next_ptr() != null_mut() {
            if self.first_ptr() == self.next_ptr() {
                let block = unsafe { self.next_blk() };
                block.beg == block.end
            } else {
                false
            }
        } else {
            true
        }
    }

    /// push a work item onto the work queue
    pub fn push<F>(&self, f: F)
    where
        F: WorkItem<'a>,
    {
        let f = Some(f);
        let len = size_of_val(&f);
        unsafe {
            self.preallocate(len);
            self.next_blk_mut().push(f);
            self.stats.enqueue();
        }
    }

    /// dispatch a single work item, returns true if additional items remain
    pub fn pump_one(&self) -> bool {

        if self.first_ptr() == null_mut() {
            return false;
        }

        assert!(!self.pumping.get());

        self.pumping.set(true);

        let more = {

            let first = unsafe { self.first_blk_mut() };

            let empty = first.exec_one();

            self.stats.execute();

            if empty {
                #[cfg(debug_assertions)]
                debug_assert!(first.is_reset());

                if self.next_ptr() != self.first_ptr() {
                    let block = self.pop_front();
                    self.push_back(block);
                    self.stats.read_rollover();
                    true
                } else {
                    false
                }
            } else {
                true
            }

        };

        self.pumping.set(false);

        more
    }

    /// dispatch all queued work items
    pub fn pump(&self) {
        while self.pump_one() {}
    }

    fn pop_front(&self) -> *mut Block {
        debug_assert!(self.first_ptr() != self.next_ptr());
        unsafe {
            self.replace_first_ptr(
                replace(
                    &mut self.first_blk_mut().next,
                    null_mut()
                )
            )
        }
    }

    fn push_back(&self, block: *mut Block) {
        debug_assert!(self.last_ptr() != null_mut());
        debug_assert!(unsafe { (*block).next } == null_mut());
        debug_assert!(unsafe { self.last_blk().next } == null_mut());
        unsafe {
            self.last_blk_mut().next = block;
            self.replace_last_ptr(block);
        }
    }

    /// ensure the block pointed to by `self.next` contains at least `len` bytes
    unsafe fn preallocate(&self, len: usize) {
        const BLOCK_LEN: usize = 4096;
        const MAX_ITEM_LEN: usize = BLOCK_LEN - size_of::<Block>();

        assert!(len <= MAX_ITEM_LEN);

        if self.next_ptr() == null_mut() {
            // first allocation from empty

            debug_assert!(self.first_ptr() == null_mut());
            debug_assert!(self.last_ptr() == null_mut());

            let new_block = Self::new_block(BLOCK_LEN);

            self.replace_first_ptr(new_block);
            self.replace_next_ptr(new_block);
            self.replace_last_ptr(new_block);

            self.stats.block_allocate();
        } else {
            // check current, then skip or grow if needed

            debug_assert!(self.first_ptr() != null_mut());
            debug_assert!(self.next_ptr() != null_mut());
            debug_assert!(self.last_ptr() != null_mut());
            debug_assert!(self.last_blk().next == null_mut());

            if self.next_blk().free_remaining() < len {
                // there is not enough room in the current block, skip to the next

                if self.next_blk().next == null_mut() {
                    // we need a new block to spill into

                    debug_assert!(self.next_ptr() == self.last_ptr());

                    self.push_back(Self::new_block(BLOCK_LEN));

                    self.stats.block_allocate();
                }

                self.replace_next_ptr(self.next_blk().next);

                self.stats.write_rollover();
            }
        }
    }

    /// allocate a new block
    fn new_block(size: usize) -> *mut Block {
        use std::alloc::{alloc, Layout};

        let layout = Layout::from_size_align(size, 32).unwrap();

        unsafe {
            let ptr = alloc(layout) as *mut Block;

            ptr.write(Block::new(size));

            ptr
        }
    }


    fn first_ptr(&self) -> *mut Block { unsafe { self.first_.get().read() } }
    fn next_ptr(&self) -> *mut Block { unsafe { self.next_.get().read() } }
    fn last_ptr(&self) -> *mut Block { unsafe { self.last_.get().read() } }

    unsafe fn replace_first_ptr(&self, new_block: *mut Block) -> *mut Block { replace(&mut*self.first_.get(), new_block) }
    unsafe fn replace_next_ptr(&self, new_block: *mut Block) -> *mut Block { replace(&mut*self.next_.get(), new_block) }
    unsafe fn replace_last_ptr(&self, new_block: *mut Block) -> *mut Block { replace(&mut*self.last_.get(), new_block) }

    unsafe fn _first_blk(&self) -> &Block { &*self.first_ptr() }
    unsafe fn next_blk(&self) -> &Block { &*self.next_ptr() }
    unsafe fn last_blk(&self) -> &Block { &*self.last_ptr() }

    unsafe fn first_blk_mut(&self) -> &mut Block { &mut *self.first_ptr() }
    unsafe fn next_blk_mut(&self) -> &mut Block { &mut *self.next_ptr() }
    unsafe fn last_blk_mut(&self) -> &mut Block { &mut *self.last_ptr() }
}

impl Block {
    pub fn new(size: usize) -> Self {
        Self {
            next: null_mut(),
            size: size,
            beg: size_of::<Block>(),
            end: size_of::<Block>(),
        }
    }

    pub fn free_remaining(&self) -> usize {
        debug_assert!(self.end >= self.beg);
        self.size - self.end
    }

    /// returns true if block is empty and write pointer is at beginning
    #[cfg(debug_assertions)]
    pub fn is_reset(&self) -> bool {
        if self.beg == self.end {
            debug_assert!(self.end == size_of::<Block>());
            true
        } else {
            false
        }
    }

    /// push a work item into block with undefined behavior if there is no room
    pub fn push<'a, F: WorkItem<'a>>(&mut self, work_item: Option<F>) {

        if std::mem::align_of::<ItemHeader>() > std::mem::size_of::<ItemHeader>() {
            panic!()
        }

        if std::mem::align_of::<Option<F>>() > std::mem::size_of::<ItemHeader>() {
            panic!()
        }

        let alloc_len = Self::align(size_of::<ItemHeader>() + size_of::<Option<F>>());

        unsafe {
            let header_ptr = self.alloc(alloc_len);
            let payload_ptr = header_ptr.offset(size_of::<ItemHeader>() as isize);

            let header_ptr = &mut *(header_ptr as *mut ItemHeader);
            let payload_ptr: *mut Option<F> = payload_ptr as *mut _;

            header_ptr.size = alloc_len;
            header_ptr.zero = 0;
            header_ptr.exec = Self::exec_impl::<F>;
            header_ptr.drop = Self::drop_impl::<F>;

            payload_ptr.write(work_item)
        }
    }

    /// execute the next item in the block, returns true if block is empty after
    pub fn exec_one(&mut self) -> bool {
        if self.beg < self.end {
            unsafe {
                let ptr = self as *mut _ as *mut u8;
                let item_header_ptr = ptr.offset(self.beg as isize);
                let item_payload_ptr = item_header_ptr.offset(size_of::<ItemHeader>() as isize);
                let item = &mut *(item_header_ptr as *mut ItemHeader);

                // assert item size falls within allocated region
                debug_assert!(self.beg + item.size <= self.end);

                self.beg += item.size;

                (item.exec)(item_payload_ptr);
            }
            if self.beg >= self.end {
                self.beg = size_of::<Block>();
                self.end = size_of::<Block>();
                true
            } else {
                false
            }
        } else {
            debug_assert!(self.beg == size_of::<Block>());
            debug_assert!(self.end == size_of::<Block>());
            true
        }
    }

    fn align(len: usize) -> usize {
        let over = len % size_of::<ItemHeader>();
        if over != 0 {
            len + size_of::<ItemHeader>() - over
        } else {
            len
        }
    }

    unsafe fn alloc(&mut self, len: usize) -> *mut u8 {
        debug_assert!(len <= (self.size - self.end)); // fits in block, and
        debug_assert!((len % size_of::<ItemHeader>()) == 0); // size is aligned

        let offset = self.end;

        self.end = self.end + len;

        let ptr = self as *mut _ as *mut u8;

        ptr.offset(offset as isize)
    }

    unsafe fn drop_impl<'a, F: WorkItem<'a>>(ptr: *mut u8) {
        std::ptr::drop_in_place(ptr as *mut Option<F>)
    }

    unsafe fn exec_impl<'a, F: WorkItem<'a>>(ptr: *mut u8) {
        let work_item = ptr as *mut Option<F>;
        let work_item = &mut *work_item;
        work_item.take().unwrap().execute();
        std::ptr::drop_in_place(work_item)
    }
}
