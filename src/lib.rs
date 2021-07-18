//! This crate provides a low overhead single threaded work queue that can
//! enqueue and execute FnOnce closures
//!
//! ```
//! use std::cell::RefCell;
//! use single_threaded_work_queue::WorkQueue;
//!
//! let value = RefCell::new(0);
//!
//! {
//!     let mut que = WorkQueue::new();
//!
//!     que.push(|| *value.borrow_mut() += 1);
//!     que.push(|| *value.borrow_mut() += 1);
//!
//!     que.pump();
//! }
//!
//! assert_eq!(*value.borrow(), 2);
//! ```

/// A run once work item.
pub trait WorkItem {

    #[cfg(feature = "dynamic_work_item")]
    type Dynamic : DynamicWorkItem;

    /// Execute the work item consuming itself.
    fn execute(self);

    /// Convert the work item into a type erasable version of itself.
    #[cfg(feature = "dynamic_work_item")]
    fn to_dynamic(self) -> Self::Dynamic;
}

/// A type-erasable work item that can only be executed one.
///
/// Because a `WorkItem` consumes itself on execution, it must be wrapped to
/// be able to be type erasable. See [WorkItem::to_dynamic]
///
/// ```
/// use single_threaded_work_queue::{WorkItem,DynWorkItem};
///
/// let boxed_work_item : Box<dyn DynWorkItem> = Box::new((||()).to_dynamic());
/// ```
#[cfg(feature = "dynamic_work_item")]
pub trait DynamicWorkItem {

    /// Execute the work item leaving it item in a "dispatched" state.
    ///
    /// # Panics
    ///
    /// This call will likely panic if the work item has already been executed.
    fn execute(&mut self);

    /// Returns if the item has aleady been dispatched.
    fn was_dispatched(&self) -> bool;
}

impl<T> WorkItem for T where T: FnOnce()->() {

    #[cfg(feature = "dynamic_work_item")]
    type Dynamic = WorkItemWrapper<T>;

    fn execute(self) { self(); }

    #[cfg(feature = "dynamic_work_item")]
    fn to_dynamic(self) -> Self::Dynamic{
        Self::Dynamic::from(self)
    }
}

#[cfg(feature = "dynamic_work_item")]
pub struct WorkItemWrapper<T>(Option<T>);

#[cfg(feature = "dynamic_work_item")]
impl<T: WorkItem> DynamicWorkItem for WorkItemWrapper<T> {
    fn execute(&mut self) {
        self.0.take().unwrap().execute()
    }
    fn was_dispatched(&self) -> bool {
        self.0.is_none()
    }
}

#[cfg(feature = "dynamic_work_item")]
impl<T> From<T> for WorkItemWrapper<T> {
    fn from(value: T) -> Self {
        Self(Some(value))
    }
}

mod work_queue;
pub use work_queue::*;


/// A low overhead work queue for `FnOnce()` work items.
///
/// Amortized constant time queue management with effectively zero heap memory
/// allocations per queued work item.
pub struct WorkQueue<'a>(AdvancedWorkQueue<'a, NopWorkQueueStats>);

impl<'a> Default for WorkQueue<'a> {
    fn default() -> Self { Self::new() }
}

impl<'a> WorkQueue<'a> {

    /// initialize a new instance of a work queue
    pub fn new() -> Self {
        Self(AdvancedWorkQueue::new())
    }

    /// create a new work queue with a initial storage capacity
    pub fn with_capacity(size: usize) -> Self {
        Self(AdvancedWorkQueue::with_capacity(size))
    }

    /// push a work item onto the work queue
    pub fn push<F>(&mut self, f: F) where F: FnOnce() + 'a {
        self.0.push(f)
    }

    /// dispatch a single work item, returns true if additional items remain
    pub fn pump_one(&mut self) -> bool {
        self.0.pump_one()
    }

    /// dispatch all queued work items
    pub fn pump(&mut self) {
        self.0.pump()
    }

}

#[cfg(test)]
mod tests {

    use std::{rc::Rc, cell::RefCell};

    use super::*;

    fn invoke(work_item: impl WorkItem) {
        work_item.execute()
    }

    #[cfg(feature = "dynamic_work_item")]
    fn invoke_dyn(work_item: &mut dyn DynamicWorkItem) {
        work_item.execute()
    }

    #[derive(Default)]
    struct Stats {
        enqueued: usize,
        executed: usize,
        block_allocates: usize,
        write_rollovers: usize,
        read_rollovers: usize,
    }

    impl WorkQueueStats for &mut Stats {
        fn enqueue(&mut self) { self.enqueued += 1 }
        fn execute(&mut self) { self.executed += 1}
        fn block_allocate(&mut self) { self.block_allocates += 1 }
        fn write_rollover(&mut self) { self.write_rollovers += 1 }
        fn read_rollover(&mut self) { self.read_rollovers += 1 }
    }

    #[test]
    fn static_invoke() {

        let mut value = 1;

        invoke(|| value += 1);

        assert_eq!(value, 2);
    }


    #[cfg(feature = "dynamic_work_item")]
    #[test]
    fn dynamic_invoke() {

        let mut value = 1;

        let mut work_item = (|| value += 1).to_dynamic();

        assert!(!work_item.was_dispatched());

        invoke_dyn(&mut work_item);

        assert!(work_item.was_dispatched());

        assert_eq!(value, 2);
    }

    #[test]
    fn simplest_que() {

        let value = Rc::new(RefCell::new(1));

        {
            let mut que = WorkQueue::new();

            que.push({
                let value = value.clone();
                move || *value.borrow_mut() += 1
            });

            que.pump();
        }

        assert_eq!(*value.borrow(), 2);
    }

    #[test]
    fn que_with_stats() {

        let mut stats = Stats::default();
        let value = Rc::new(RefCell::new(0));

        {
            let mut que = AdvancedWorkQueue::with_stats(&mut stats);

            for _ in 0..2 {
                que.push({
                    let value = value.clone();
                    move || *value.borrow_mut() += 1
                });
            }

            que.pump();
        }

        assert_eq!(*value.borrow(), 2);
        assert_eq!(stats.block_allocates, 1);
        assert_eq!(stats.enqueued, 2);
        assert_eq!(stats.executed, 2);
    }

    #[test]
    fn que_multi_block() {

        use std::{rc::Rc, cell::RefCell};

        let mut stats = Stats::default();
        let value = Rc::new(RefCell::new(0));

        struct BigRef<const N: usize>{
            value: Rc<RefCell<i32>>,
            _baggage: [u8;N],
        }

        impl<const N: usize> BigRef<N> {
            fn new(value: &Rc<RefCell<i32>>) -> Self {
                Self { value: value.clone(), _baggage: unsafe { std::mem::zeroed() } }
            }
            fn inc(&self) {
                *self.value.borrow_mut() += 1
            }
        }

        {
            let mut que = AdvancedWorkQueue::with_stats(&mut stats);

            const BIG : usize = 3072;
            const SMALL : usize = 768;

            for _ in 0..4 {
                que.push({
                    let r = BigRef::<BIG>::new(&value);
                    move || r.inc()
                });
            }

            que.pump();

            for _ in 0..8 {
                que.push({
                    let r = BigRef::<SMALL>::new(&value);
                    move || r.inc()
                });
            }

            que.pump();
        }

        assert_eq!(*value.borrow(), 12);
        assert_eq!(stats.block_allocates, 4);
        assert_eq!(stats.write_rollovers, 4);
        assert_eq!(stats.read_rollovers, 4);
        assert_eq!(stats.enqueued, 12);
        assert_eq!(stats.executed, 12);
    }
}
