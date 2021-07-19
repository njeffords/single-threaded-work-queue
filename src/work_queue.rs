
use std::{marker::PhantomData, mem::{size_of,size_of_val}, ptr::null_mut};

use super::*;

#[repr(C)]
struct Block{

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
struct ItemHeader{
    size: usize,
    zero: usize,
    exec: unsafe fn(*mut u8),
    drop: unsafe fn(*mut u8),
}

/// trait for types that can track work queue statistics
pub trait WorkQueueStats{

    /// single work item enqueued
    fn enqueue(&mut self) {}

    /// single work item executed
    fn execute(&mut self) {}

    /// new queue block allocated
    fn block_allocate(&mut self) {}

    /// block write pointer move to new block
    fn write_rollover(&mut self) {}

    /// block read pointer moved to new block
    fn read_rollover(&mut self) {}
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
pub struct AdvancedWorkQueue<'a, S:WorkQueueStats> {
    first: *mut Block,
    next: *mut Block,
    last: *mut Block,
    stats: S,
    _store: PhantomData<[&'a ()]>
}

impl<'a, S:WorkQueueStats+Default> Default for AdvancedWorkQueue<'a, S> {
    fn default() -> Self { Self::new() }
}

impl<'a, S:WorkQueueStats+Default> AdvancedWorkQueue<'a, S> {

    /// create a new work queue without a buffer
    pub fn new() -> Self {
        Self::with_stats(S::default())
    }

    /// create a new work queue with an initial storage capacity
    pub fn with_capacity(size: usize) -> Self {
        Self::with_stats_and_capacity(S::default(), size)
    }
}

impl<'a, S:WorkQueueStats> AdvancedWorkQueue<'a, S> {

    /// create a new work queue with the specified stats tracker and without a buffer
    pub fn with_stats(stats: S) -> Self {
        Self{
            first: null_mut(),
            next: null_mut(),
            last: null_mut(),
            stats,
            _store: PhantomData::default(),
        }
    }

    /// create a new work queue with the specified stats tracker and an initial
    /// storage capacity
    pub fn with_stats_and_capacity(stats: S, size: usize) -> Self {
        let block = Self::new_block(size);
        Self{
            first: block,
            next: block,
            last: block,
            stats,
            _store: PhantomData::default(),
        }
    }

    /// push a work item onto the work queue
    pub fn push<F>(&mut self, f: F) where F: WorkItem + 'a {
        let f = Some(f);
        let len = size_of_val(&f);
        unsafe {
            self.preallocate(len);
            (*self.next).push(f);
            self.stats.enqueue();
        }
    }

    /// dispatch a single work item, returns true if additional items remain
    pub fn pump_one(&mut self) -> bool {

        if self.first == null_mut() {
            return false;
        }

        let first = unsafe {&mut*self.first};

        let empty = first.exec_one();

        self.stats.execute();

        if empty {
            debug_assert!(first.is_reset());

            if self.next != self.first {
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
    }

    /// dispatch all queued work items
    pub fn pump(&mut self) { while self.pump_one() {} }

    fn pop_front(&mut self) -> *mut Block {
        debug_assert!(self.first != self.next);
        let popped = self.first;
        self.first = unsafe {(*self.first).next};
        unsafe{(*popped).next = null_mut()};
        popped
    }

    fn push_back(&mut self, block: *mut Block) {
        debug_assert!(self.last != null_mut());
        debug_assert!(unsafe{(*block).next} == null_mut());
        debug_assert!(unsafe{(*self.last).next} == null_mut());
        unsafe {(*self.last).next = block };
        self.last = block;
    }

    /// ensure the block pointed to by `self.next` contains at least `len` bytes
    unsafe fn preallocate(&mut self, len: usize) {

        const BLOCK_LEN: usize = 4096;
        const MAX_ITEM_LEN: usize = BLOCK_LEN - size_of::<Block>();

        assert!(len <= MAX_ITEM_LEN);

        if self.next == null_mut() {

            // first allocation from empty

            debug_assert!(self.first == null_mut());
            debug_assert!(self.last == null_mut());

            let ptr = Self::new_block(BLOCK_LEN);

            self.first = ptr;
            self.next = ptr;
            self.last = ptr;

            self.stats.block_allocate();

        } else {

            // check current, then skip or grow if needed

            debug_assert!(self.first != null_mut());
            debug_assert!(self.next != null_mut());
            debug_assert!(self.last != null_mut());
            debug_assert!((*self.last).next == null_mut());

            if (*self.next).free_remaining() < len {

                // there is not enough room in the current block, skip to the next

                if (*self.next).next == null_mut() {

                    // we need a new block to spill into

                    debug_assert!(self.next == self.last);

                    self.push_back(Self::new_block(BLOCK_LEN));

                    self.stats.block_allocate();
                }

                self.next = (*self.next).next;

                self.stats.write_rollover();
            }
        }
    }

    /// allocate a new block
    fn new_block(size: usize) -> *mut Block {

        use std::alloc::{alloc,Layout};

        let layout = Layout::from_size_align(size,32).unwrap();

        unsafe {

            let ptr = alloc(layout) as *mut Block;

            ptr.write(Block::new(size));

            ptr
        }
    }
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
    pub fn push<F:WorkItem>(&mut self, work_item: Option<F>) {

        let alloc_len = Self::align(size_of::<ItemHeader>() + size_of::<Option<F>>());

        unsafe {

            let header_ptr = self.alloc(alloc_len);
            let payload_ptr = header_ptr.offset(size_of::<ItemHeader>() as isize);

            let header_ptr = &mut*(header_ptr as *mut ItemHeader);
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
                let item = &mut*(item_header_ptr as *mut ItemHeader);

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
        let over = len % size_of::<ItemHeader> ();
        if over != 0 {
            len + size_of::<ItemHeader> () - over
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

    unsafe fn drop_impl<F:WorkItem>(ptr: *mut u8) {
        std::ptr::drop_in_place(ptr as *mut Option<F>)
    }

    unsafe fn exec_impl<F:WorkItem>(ptr: *mut u8) {
        let work_item = ptr as *mut Option<F>;
        let work_item = &mut*work_item;
        work_item.take().unwrap().execute();
        std::ptr::drop_in_place(work_item)
    }
}