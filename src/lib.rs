#![doc = include_str!("../README.md")]

/// A run once work item.
pub trait WorkItem<'a> {

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

impl<'a, T> WorkItem<'a> for T where T: FnOnce()->() + 'a {

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

    /// return true of there are no items in the queue
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// create a new work queue with a initial storage capacity
    pub fn with_capacity(size: usize) -> Self {
        Self(AdvancedWorkQueue::with_capacity(size))
    }

    /// push a work item onto the work queue
    pub fn push<F>(&self, f: F) where F: FnOnce() + 'a {
        self.0.push(f)
    }

    /// dispatch a single work item, returns true if additional items remain
    pub fn pump_one(&self) -> bool {
        self.0.pump_one()
    }

    /// dispatch all queued work items
    pub fn pump(&self) {
        self.0.pump()
    }

}

#[cfg(test)]
mod tests {

    use std::{rc::Rc, cell::{Cell, RefCell}};

    use super::*;

    fn invoke<'a>(work_item: impl WorkItem<'a>) {
        work_item.execute()
    }

    #[cfg(feature = "dynamic_work_item")]
    fn invoke_dyn(work_item: &mut dyn DynamicWorkItem) {
        work_item.execute()
    }

    #[derive(Default)]
    struct Stat(Cell<usize>);

    impl Stat {
        fn inc(&self) { self.0.set(self.0.get() + 1) }
        fn get(&self) -> usize { self.0.get() }
    }

    #[derive(Default)]
    struct Stats {
        enqueued: Stat,
        executed: Stat,
        block_allocates: Stat,
        write_rollovers: Stat,
        read_rollovers: Stat,
    }

    impl WorkQueueStats for &Stats {
        fn enqueue(&self) { self.enqueued.inc() }
        fn execute(&self) { self.executed.inc() }
        fn block_allocate(&self) { self.block_allocates.inc() }
        fn write_rollover(&self) { self.write_rollovers.inc() }
        fn read_rollover(&self) { self.read_rollovers.inc() }
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
    fn pump_empty() {
        let que = WorkQueue::new();
        assert!(!que.pump_one());
    }

    #[test]
    fn simplest_que() {

        let value = Rc::new(RefCell::new(1));

        {
            let que = WorkQueue::new();

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

        let stats = Stats::default();
        let value = Rc::new(RefCell::new(0));

        {
            let que = AdvancedWorkQueue::with_stats(&stats);

            assert!(que.is_empty());

            for _ in 0..2 {
                que.push({
                    let value = value.clone();
                    move || *value.borrow_mut() += 1
                });
            }

            assert!(!que.is_empty());

            que.pump();

            assert!(que.is_empty());
        }

        assert_eq!(*value.borrow(), 2);
        assert_eq!(stats.block_allocates.get(), 1);
        assert_eq!(stats.enqueued.get(), 2);
        assert_eq!(stats.executed.get(), 2);
    }

    #[test]
    fn que_multi_block() {

        use std::{rc::Rc, cell::RefCell};

        let stats = Stats::default();
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
            let que = AdvancedWorkQueue::with_stats(&stats);

            const BIG : usize = 3072;
            const SMALL : usize = 768;

            assert!(que.is_empty());

            for _ in 0..4 {
                que.push({
                    let r = BigRef::<BIG>::new(&value);
                    move || r.inc()
                });
            }

            assert!(!que.is_empty());

            for _ in 0..3 {
                que.pump_one();
            }

            assert!(!que.is_empty());

            que.pump_one();

            assert!(que.is_empty());

            for _ in 0..8 {
                que.push({
                    let r = BigRef::<SMALL>::new(&value);
                    move || r.inc()
                });
            }

            assert!(!que.is_empty());

            que.pump();

            assert!(que.is_empty());
        }

        assert_eq!(*value.borrow(), 12);
        assert_eq!(stats.block_allocates.get(), 4);
        assert_eq!(stats.write_rollovers.get(), 4);
        assert_eq!(stats.read_rollovers.get(), 4);
        assert_eq!(stats.enqueued.get(), 12);
        assert_eq!(stats.executed.get(), 12);
    }

    #[test]
    fn queue_in_queue() {

        let value = RefCell::new(0);

        {
            let stats = Stats::default();
            let que = AdvancedWorkQueue::with_stats(&stats);

            que.push(|| que.push(|| *value.borrow_mut() += 1));

            que.pump();
        }

        assert_eq!(*value.borrow(), 1);

    }
}
