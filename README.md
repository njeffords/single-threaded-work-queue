# Single thread work queue

This crate provides a low overhead single threaded work queue that can
enqueue and execute FnOnce closures

## Examples

```rust
use std::cell::RefCell;
use single_threaded_work_queue::WorkQueue;

let value = RefCell::new(0);

{
    let mut que = WorkQueue::new();

    que.push(|| *value.borrow_mut() += 1);
    que.push(|| *value.borrow_mut() += 1);

    que.pump();
}

assert_eq!(*value.borrow(), 2);
```

Note that the closures do not require 'static, only that things they match
or exceed the lifetime of the work queue itself. This allows capturing a
reference to the work queue in order to post more work as in the following:

```rust
use std::cell::RefCell;
use single_threaded_work_queue::WorkQueue;

let value = RefCell::new(0);

{
    let mut que = WorkQueue::new();

    que.push(|| {
        *value.borrow_mut() += 1;
        que.push(|| *value.borrow_mut() += 1);
    });

    que.pump();
}

assert_eq!(*value.borrow(), 2);
```

## Performance

Pushing and executing work items are  done in amortized constant time. Once
warm, it should settle to constant time. Execution is cache friendly as items
are layed out in chunks of continuous bytes with no gaps.

## Safety

This crate requires non-trivial unsafety in its implementation. The
maintainer(s) of this crate assert that its public interface is safe. The
intention of this section is to provides details that may aid in the analysis
required to verify that assertion.

Conceptually, the work queue is a circular buffer of bytes that work items are
are written into to await execution. The structure is made of two layers, one
layer is an allocator for blocks of memory containing the work items and the
other is the individual work items. The outer layer is never visible and always
left consistent at the end of a call or prior to transferring execution to a
work item's handler. A work item on the queue can only have a live reference
externally visible while it is executing. Since it is not legal to execute more
than one item at a time, there can only be one of these references outstanding.
