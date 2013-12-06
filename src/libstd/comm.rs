// Copyright 2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Rust Communication Primitives
//!
//! Rust makes it very difficult to share data among tasks to prevent race
//! conditions and to improve parallelism, but there is often a need for
//! communication between concurrent tasks. The primitives defined in this
//! module are the building blocks for synchronization in rust.
//!
//! This module currently provides three main types:
//!
//! * `Chan`
//! * `Port`
//! * `SharedChan`
//!
//! The `Chan` and `SharedChan` types are used to send data to a `Port`. A
//! `SharedChan` is clone-able such that many tasks can send simultaneously to
//! one receiving port. These communication primitives are *task blocking*, not
//! *thread blocking*. This means that if one task is blocked on a channel,
//! other tasks can continue to make progress.
//!
//! Rust channels can be used as if they have an infinite internal buffer. What
//! this means is that the `send` operation will never block. `Port`s, on the
//! other hand, will block the task if there is no data to be received.
//!
//! ## Failure Propagation
//!
//! In addition to being a core primitive for communicating in rust, channels
//! and ports are the points at which failure is propagated among tasks.
//! Whenever the one half of channel is closed, the other half will have its
//! next operation `fail!`. The purpose of this is to allow propagation of
//! failure among tasks that are linked to one another via channels.
//!
//! There are methods on all of `Chan`, `SharedChan`, and `Port` to perform
//! their respective operations without failing, however.
//!
//! ## Outside the Runtime
//!
//! All channels and ports work seamlessly inside and outside of the rust
//! runtime. This means that code may use channels to communicate information
//! inside and outside of the runtime. For example, if rust were embedded as an
//! FFI module in another application, the rust runtime would probably be
//! running in its own external thread pool. Channels created can communicate
//! from the native application threads to the rust threads through the use of
//! native mutexes and condition variables.
//!
//! What this means is that if a native thread is using a channel, execution
//! will be blocked accordingly by blocking the OS thread.
//!
//! # Example
//!
//!     // Create a simple streaming channel
//!     let (port, chan) = Chan::new();
//!     do spawn {
//!         chan.send(10);
//!     }
//!     assert_eq!(port.recv(), 10);
//!
//!     // Create a shared channel which can be sent along from many tasks
//!     let (port, chan) = SharedChan::new();
//!     for i in range(0, 10) {
//!         let chan = chan.clone();
//!         do spawn {
//!             chan.send(i);
//!         }
//!     }
//!
//!     for _ in range(0, 10) {
//!         let j = port.recv();
//!         assert!(0 <= j && j < 10);
//!     }
//!
//!     // The call to recv() will fail!() because the channel has already hung
//!     // up (or been deallocated)
//!     let (port, chan) = Chan::new();
//!     drop(chan);
//!     port.recv();

// A description of how Rust's channel implementation works
//
// Channels are supposed to be the basic building block for all other
// concurrent primitives that are used in Rust. As a result, the channel type
// needs to be highly optimized, flexible, and broad enough for use everywhere.
//
// The choice of implementation of all channels is to be built on lock-free data
// structures. The channels themselves are then consequently also lock-free data
// structures. As always with lock-free code, this is a very "here be dragons"
// territory, especially because I'm unaware of any academic papers which have
// gone into great length about channels of these flavors.
//
// ## Flavors of channels
//
// Rust channels come in two flavors: streams and shared channels. A stream has
// one sender and one receiver while a shared channel could have multiple
// receivers. This choice heavily influences the design of the protocol set
// forth for both senders/receivers.
//
// ## Concurrent queues
//
// The basic idea of Rust's Chan/Port types is that send() never blocks, but
// recv() obviously blocks. This means that under the hood there must be some
// shared and concurrent queue holding all of the actual data.
//
// With two flavors of channels, two flavors of queues are also used. We have
// chosen to use queues from a well-known author which are abbreviated as SPSC
// and MPSC (single producer, single consumer and multiple producer, single
// consumer). SPSC queues are used for streams while MPSC queues are used for
// shared channels.
//
// ### SPSC optimizations
//
// The SPSC queue found online is essentially a linked list of nodes where one
// half of the nodes are the "queue of data" and the other half of nodes are a
// cache of unused nodes. The unused nodes are used such that an allocation is
// not required on every push() and a free doesn't need to happen on every
// pop().
//
// As found online, however, the cache of nodes is of an infinite size. This
// means that if a channel at one point in its life had 50k items in the queue,
// then the queue will always have the capacity for 50k items. I believed that
// this was an unnecessary limitation of the implementation, so I have altered
// the queue to optionally have a bound on the cache size.
//
// By default, streams will this unbounded SPSC queue with a small-ish cache
// size. The hope is that the cache is still large enough to have very fast
// send() operations while not too large such that millions of channels can
// coexist at once.
//
// ### MPSC optimizations
//
// Right now the MPSC queue has not been optimized. Like the SPSC queue, it uses
// a linked list under the hood to earn its unboundedness, but I have not put
// forth much effort into having a cache of nodes similar to the SPSC queue.
//
// For now, I believe that this is "ok" because shared channels are not the most
// common type, but soon we may wish to revisit this queue choice and determine
// another candidate for backend storage of shared channels.
//
// ## Overview of the Implementation
//
// Now that there's a little background on the concurrent queues used, it's
// worth going into much more detail about the channels themselves. The basic
// pseudocode for a send/recv are:
//
//
//      send(t)                             recv()
//        queue.push(t)                       return if queue.pop()
//        if increment() == -1                deschedule {
//          wakeup()                            if decrement() > 0
//                                                cancel_deschedule()
//                                            }
//                                            queue.pop()
//
// As mentioned before, there are no locks in this implementation, only atomic
// instructions are used.
//
// ### The internal atomic counter
//
// Every channel/port/shared channel have a shared counter with their
// counterparts to keep track of the size of the queue. This counter is used to
// abort descheduling by the receiver and to know when to wake up on the sending
// side.
//
// As seen in the pseudocode, senders will increment this count and receivers
// will decrement the count. The theory behind this is that if a sender sees a
// -1 count, it will wake up the receiver, and if the receiver sees a 1+ count,
// then it doesn't need to block.
//
// The recv() method has a beginning call to pop(), and if successful, it needs
// to decrement the count. It is a crucial implementation detail that this
// decrement does *not* happen to the shared counter. If this were the case,
// then it would be possible for the counter to be very negative when there were
// no receivers waiting, in which case the senders would have to determine when
// it was actually appropriate to wake up a receiver.
//
// Instead, the "steal count" is kept track of separately (not atomically
// because it's only used by ports), and then the decrement() call when
// descheduling will lump in all of the recent steals into one large decrement.
//
// The implication of this is that if a sender sees a -1 count, then there's
// guaranteed to be a waiter waiting!
//
// ## Native Implementation
//
// A major goal of these channels is to work seamlessly on and off the runtime.
// All of the previous race conditions have been worded in terms of
// scheduler-isms (which is obviously not available without the runtime).
//
// For now, native usage of channels (off the runtime) will fall back onto
// mutexes/cond vars for descheduling/atomic decisions. The no-contention path
// is still entirely lock-free, the "deschedule" blocks above are surrounded by
// a mutex and the "wakeup" blocks involve grabbing a mutex and signaling on a
// condition variable.
//
// ## Select
//
// Being able to support selection over channels has greatly influenced this
// design, and not only does selection need to work inside the runtime, but also
// outside the runtime.
//
// The implementation is fairly straightforward. The goal of select() is not to
// return some data, but only to return which channel can receive data without
// blocking. The implementation is essentially the entire blocking procedure
// followed by an increment as soon as its woken up. The cancellation procedure
// involves an increment and swapping out of to_wake to acquire ownership of the
// task to unblock.
//
// Sadly this current implementation requires multiple allocations, so I have
// seen the throughput of select() be much worse than it should be. I do not
// believe that there is anything fundamental which needs to change about these
// channels, however, in order to support a more efficient select().
//
// # Conclusion
//
// And now that you've seen all the races that I found and attempted to fix,
// here's the code for you to find some more!

use cast;
use clone::Clone;
use container::Container;
use int;
use iter::{Iterator, DoubleEndedIterator};
use kinds::Send;
use ops::Drop;
use option::{Option, Some, None};
use unstable::atomics::{AtomicInt, SeqCst};
use vec::{ImmutableVector, OwnedVector};

use spsc = rt::spsc_queue;
use mpsc = rt::mpsc_queue;

use self::imp::{TaskHandle, TaskData, BlockingContext};

///////////////////////////////////////////////////////////////////////////////
//
// One of the major goals behind this channel implementation is to work
// seamlessly on and off the runtime. This also means that the code isn't
// littered with "if is_green() { ... } else { ... }". Right now, the rest of
// the runtime isn't quite ready to for this abstraction to be done very nicely,
// so the conditional "if green" blocks are all contained in this inner module.
//
// The goal of this module is to mirror what the runtime "should be", not the
// state that it is currently in today. You'll notice that there is no mention
// of schedulers or is_green inside any of the channel code, it is currently
// entirely contained in this one module.
//
// In the ideal world, nothing in this module exists and it is all implemented
// elsewhere in the runtime (in the proper location). All of this code is
// structured in order to easily refactor this to the correct location whenever
// we have the trait objects in place to serve as the boundary of the
// abstraction.

mod imp {
    use iter::{range, Iterator};
    use ops::Drop;
    use option::{Some, None, Option};
    use rt::local::Local;
    use rt::sched::{SchedHandle, Scheduler, TaskFromFriend};
    use rt::thread::Thread;
    use rt;
    use unstable::mutex::Mutex;
    use unstable::sync::UnsafeArc;

    // A task handle is a method of waking up a blocked task. The handle itself
    // is completely opaque and only has a wake() method defined on it. This
    // method will wake the method regardless of the context of the thread which
    // is currently calling wake().
    //
    // This abstraction should be able to be created when putting a task to
    // sleep. This should basically be a method on whatever the local Task is,
    // consuming the local Task.

    pub struct TaskHandle {
        priv inner: TaskRepr
    }
    enum TaskRepr {
        Green(rt::BlockedTask, *mut SchedHandle),
        Native(NativeWakeupStyle),
    }
    enum NativeWakeupStyle {
        ArcWakeup(UnsafeArc<Mutex>),    // shared mutex to synchronize on
        LocalWakeup(*mut Mutex),        // synchronize on the task-local mutex
    }

    impl TaskHandle {
        // Signal that this handle should be woken up. The `can_resched`
        // argument indicates whether the current task could possibly be
        // rescheduled or not. This does not have a lot of meaning for the
        // native case, but for an M:N case it indicates whether a context
        // switch can happen or not.
        pub fn wake(self, can_resched: bool) {
            match self.inner {
                Green(task, handle) => {
                    // If we have a local scheduler, then use that to run the
                    // blocked task, otherwise we can use the handle to send the
                    // task back to its home.
                    if rt::in_green_task_context() {
                        if can_resched {
                            task.wake().map(Scheduler::run_task);
                        } else {
                            let mut s: ~Scheduler = Local::take();
                            s.enqueue_blocked_task(task);
                            Local::put(s);
                        }
                    } else {
                        let task = match task.wake() {
                            Some(task) => task, None => return
                        };
                        // XXX: this is not an easy section of code to refactor.
                        //      If this handle is owned by the Task (which it
                        //      should be), then this would be a use-after-free
                        //      because once the task is pushed onto the message
                        //      queue, the handle is gone.
                        //
                        //      Currently the handle is instead owned by the
                        //      Port/Chan pair, which means that because a
                        //      channel is invoking this method the handle will
                        //      continue to stay alive for the entire duration
                        //      of this method. This will require thought when
                        //      moving the handle into the task.
                        unsafe { (*handle).send(TaskFromFriend(task)) }
                    }
                }

                // Note that there are no use-after-free races in this code. In
                // the arc-case, we own the lock, and in the local case, we're
                // using a lock so it's guranteed that they aren't running while
                // we hold the lock.
                Native(ArcWakeup(lock)) => {
                    unsafe {
                        let lock = lock.get();
                        (*lock).lock();
                        (*lock).signal();
                        (*lock).unlock();
                    }
                }
                Native(LocalWakeup(lock)) => {
                    unsafe {
                        (*lock).lock();
                        (*lock).signal();
                        (*lock).unlock();
                    }
                }
            }
        }

        // Trashes handle to this task. This ensures that necessary memory is
        // deallocated, and there may be some extra assertions as well.
        pub fn trash(self) {
            match self.inner {
                Green(task, _) => task.assert_already_awake(),
                Native(..) => {}
            }
        }
    }

    // This structure is an abstraction of what should be stored in the local
    // task itself. This data is currently stored inside of each channel, but
    // this should rather be stored in each task (and channels will still
    // continue to lazily initialize this data).

    pub struct TaskData {
        priv handle: Option<SchedHandle>,
        priv lock: Mutex,
    }

    impl TaskData {
        pub fn new() -> TaskData {
            TaskData {
                handle: None,
                lock: unsafe { Mutex::empty() },
            }
        }
    }

    impl Drop for TaskData {
        fn drop(&mut self) {
            unsafe { self.lock.destroy() }
        }
    }

    // Now this is the really fun part. This is where all the M:N/1:1-agnostic
    // along with recv/select-agnostic blocking information goes. A "blocking
    // context" is really just a stack-allocated structure (which is probably
    // fine to be a stack-trait-object).
    //
    // This has some particularly strange interfaces, but the reason for all
    // this is to support selection/recv/1:1/M:N all in one bundle.

    pub struct BlockingContext<'a> {
        priv inner: BlockingRepr<'a>
    }

    enum BlockingRepr<'a> {
        GreenBlock(rt::BlockedTask, &'a mut Scheduler),
        NativeBlock(Option<UnsafeArc<Mutex>>),
    }

    impl<'a> BlockingContext<'a> {
        // Creates one blocking context. The data provided should in theory be
        // acquired from the local task, but it is instead acquired from the
        // channel currently.
        //
        // This function will call `f` with a blocking context, plus the data
        // that it is given. This function will then return whether this task
        // should actually go to sleep or not. If `true` is returned, then this
        // function does not return until someone calls `wake()` on the task.
        // If `false` is returned, then this function immediately returns.
        //
        // # Safety note
        //
        // Note that this stack closure may not be run on the same stack as when
        // this function was called. This means that the environment of this
        // stack closure could be unsafely aliased. This is currently prevented
        // through the guarantee that this function will never return before `f`
        // finishes executing.
        pub fn one(data: &mut TaskData,
                   f: |BlockingContext, &mut TaskData| -> bool) {
            if rt::in_green_task_context() {
                let sched: ~Scheduler = Local::take();
                sched.deschedule_running_task_and_then(|sched, task| {
                    let ctx = BlockingContext { inner: GreenBlock(task, sched) };
                    f(ctx, data);
                });
            } else {
                unsafe { data.lock.lock(); }
                let ctx = BlockingContext { inner: NativeBlock(None) };
                if f(ctx, data) {
                    unsafe { data.lock.wait(); }
                }
                unsafe { data.lock.unlock(); }
            }
        }

        // Creates many blocking contexts. The intended use case for this
        // function is selection over a number of ports. This will create `amt`
        // blocking contexts, yielding them to `f` in turn. If `f` returns
        // false, then this function aborts and returns immediately. If `f`
        // repeatedly returns `true` `amt` times, then this function will block.
        pub fn many(amt: uint, f: |BlockingContext| -> bool) {
            if rt::in_green_task_context() {
                let sched: ~Scheduler = Local::take();
                sched.deschedule_running_task_and_then(|sched, task| {
                    for handle in task.make_selectable(amt) {
                        let ctx = BlockingContext {
                            inner: GreenBlock(handle, sched)
                        };
                        if !f(ctx) { break }
                    }
                });
            } else {
                // In the native case, our decision to block must be shared
                // amongst all of the channels. It may be possible to
                // stack-allocate this mutex (instead of putting it in an
                // UnsafeArc box), but for now in order to prevent
                // use-after-free trivially we place this into a box and then
                // pass that around.
                unsafe {
                    let mtx = UnsafeArc::new(Mutex::new());
                    (*mtx.get()).lock();
                    let success = range(0, amt).all(|_| {
                        f(BlockingContext {
                            inner: NativeBlock(Some(mtx.clone()))
                        })
                    });
                    if success {
                        (*mtx.get()).wait();
                    }
                    (*mtx.get()).unlock();
                }
            }
        }

        // This function will consume this BlockingContext, and optionally block
        // if according to the atomic `decision` function. The semantics of this
        // functions are:
        //
        //  * `slot` is required to be a `None`-slot (which is owned by the
        //    channel)
        //  * The `slot` will be filled in with a blocked version of the current
        //    task (with `wake`-ability if this function is successful).
        //  * If the `decision` function returns true, then this function
        //    immediately returns having relinquished ownership of the task.
        //  * If the `decision` function returns false, then the `slot` is reset
        //    to `None` and the task is re-scheduled if necessary (remember that
        //    the task will not resume executing before the outer `one` or
        //    `many` function has returned.
        //
        // This function will return whether the blocking occurred or not.
        pub fn block(self,
                     data: &mut TaskData,
                     slot: &mut Option<TaskHandle>,
                     decision: || -> bool) -> bool {
            assert!(slot.is_none());
            match self.inner {
                GreenBlock(task, sched) => {
                    if data.handle.is_none() {
                        data.handle = Some(sched.make_handle());
                    }
                    let handle = data.handle.get_mut_ref() as *mut SchedHandle;
                    *slot = Some(TaskHandle { inner: Green(task, handle) });

                    if !decision() {
                        match slot.take_unwrap().inner {
                            Green(task, _) => sched.enqueue_blocked_task(task),
                            Native(..) => {}
                        }
                        false
                    } else {
                        true
                    }
                }
                NativeBlock(shared) => {
                    *slot = Some(TaskHandle {
                        inner: Native(match shared {
                            Some(arc) => ArcWakeup(arc),
                            None => LocalWakeup(&mut data.lock as *mut Mutex),
                        })
                    });

                    if !decision() {
                        *slot = None;
                        false
                    } else {
                        true
                    }
                }
            }
        }
    }

    // Agnostic method of forcing a yield of the current task
    pub fn yield_now() {
        if rt::in_green_task_context() {
            let sched: ~Scheduler = Local::take();
            sched.yield_now();
        } else {
            Thread::yield_now();
        }
    }

    // Agnostic method of "maybe yielding" in order to provide fairness
    pub fn maybe_yield() {
        if rt::in_green_task_context() {
            let sched: ~Scheduler = Local::take();
            sched.maybe_yield();
        } else {
            // the OS decides fairness, nothing for us to do.
        }
    }
}

///////////////////////////////////////////////////////////////////////////////
// Helper type to abstract ports for channels and shared channels
///////////////////////////////////////////////////////////////////////////////

enum Consumer<T> {
    SPSC(spsc::Consumer<T, Packet>),
    MPSC(mpsc::Consumer<T, Packet>),
}

impl<T: Send> Consumer<T>{
    unsafe fn packet(&self) -> *mut Packet {
        match *self {
            SPSC(ref c) => c.packet(),
            MPSC(ref c) => c.packet(),
        }
    }
}

///////////////////////////////////////////////////////////////////////////////
// Selection
///////////////////////////////////////////////////////////////////////////////

/// Performs a selection of events over an array of ports. This will block
/// waiting for any activity on any of the ports provided.
///
/// The returned value is the index at which activity has occurred. Note that
/// activity can either include disconnection or receiving data. If any of the
/// ports are originally closed, then this function will return immediately.
///
/// The returned index is the smallest index at which a port has activity.
pub fn select<T: Send>(ports: &[&Port<T>]) -> uint {
    assert!(ports.len() > 0);
    for (i, p) in ports.iter().enumerate() {
        if p.can_recv() {
            return i;
        }
    }

    let mut ready_index = ports.len();
    let mut iter = ports.iter().enumerate();

    BlockingContext::many(ports.len(), |ctx| {
        let (i, port) = iter.next().unwrap();
        unsafe {
            let packet = port.queue.packet();
            if !ctx.block(&mut (*packet).data,
                          &mut (*packet).to_wake,
                          || (*packet).decrement()) {
                (*packet).abort_selection(false);
                ready_index = i;
                false
            } else {
                true
            }
        }
    });

    let i = ports.slice_to(ready_index).iter();
    for (i, port) in i.enumerate().invert() {
        unsafe {
            let packet = port.queue.packet();
            if (*packet).abort_selection(true) {
                ready_index = i;
            }
        }
    }
    assert!(ready_index < ports.len());
    return ready_index;
}

///////////////////////////////////////////////////////////////////////////////
// Public structs
///////////////////////////////////////////////////////////////////////////////

/// The receiving-half of Rust's channel type. This half can only be owned by
/// one task
pub struct Port<T> {
    priv queue: Consumer<T>,
}

/// An iterator over messages received on a port, this iterator will block
/// whenever `next` is called, waiting for a new message, and `None` will be
/// returned when the corresponding channel has hung up.
pub struct PortIterator<'a, T> {
    priv port: &'a Port<T>
}

/// The sending-half of Rust's channel type. This half can only be owned by one
/// task
pub struct Chan<T> {
    priv queue: spsc::Producer<T, Packet>,
}

/// The sending-half of Rust's channel type. This half can be shared among many
/// tasks by creating copies of itself through the `clone` method.
pub struct SharedChan<T> {
    priv queue: mpsc::Producer<T, Packet>,
}

///////////////////////////////////////////////////////////////////////////////
// Internal struct definitions
///////////////////////////////////////////////////////////////////////////////

struct Packet {
    cnt: AtomicInt, // How many items are on this channel
    steals: int,    // How many times has a port received without blocking?
    to_wake: Option<TaskHandle>, // Task to wake up

    data: TaskData,

    // This lock is used to wake up native threads blocked in select. The
    // `lock` field is not used because the thread blocking in select must
    // block on only one mutex.
    //selection_lock: Option<UnsafeArc<Mutex>>,

    // The number of channels which are currently using this packet. This is
    // used to reference count shared channels.
    channels: AtomicInt,
}

///////////////////////////////////////////////////////////////////////////////
// All implementations -- the fun part
///////////////////////////////////////////////////////////////////////////////

static DISCONNECTED: int = int::min_value;
static RESCHED_FREQ: int = 200;

impl Packet {
    fn new() -> Packet {
        Packet {
            cnt: AtomicInt::new(0),
            steals: 0,
            to_wake: None,
            data: TaskData::new(),
            //selection_lock: None,
            channels: AtomicInt::new(1),
        }
    }

    // Increments the channel size count, preserving the disconnected state if
    // the other end has disconnected.
    fn increment(&mut self) -> int {
        match self.cnt.fetch_add(1, SeqCst) {
            DISCONNECTED => {
                self.cnt.store(DISCONNECTED, SeqCst);
                DISCONNECTED
            }
            n => n
        }
    }

    // Decrements the reference count of the channel, returning whether the task
    // should block or not. This assumes that the task is ready to sleep in that
    // the `to_wake` field has already been filled in. Once this decrement
    // happens, the task could wake up on the other end.
    //
    // From an implementation perspective, this is also when our "steal count"
    // gets merged into the "channel count". Our steal count is reset to 0 after
    // this function completes.
    //
    // As with increment(), this preserves the disconnected state if the
    // channel is disconnected.
    fn decrement(&mut self) -> bool {
        let steals = self.steals;
        self.steals = 0;
        match self.cnt.fetch_sub(1 + steals, SeqCst) {
            DISCONNECTED => {
                self.cnt.store(DISCONNECTED, SeqCst);
                false
            }
            n => {
                assert!(n >= 0);
                n - steals <= 0
            }
        }
    }

    // Aborts the selection process for a port. This happens as part of select()
    // once the task has reawoken. This will place the channel back into a
    // consistent state which is ready to be received from again.
    //
    // The method of doing this is a little subtle. These channels have the
    // invariant that if -1 is seen, then to_wake is always Some(..) and should
    // be woken up. This aborting process at least needs to add 1 to the
    // reference count, but that is not guaranteed to make the count positive
    // (our steal count subtraction could mean that after the addition the
    // channel count is still negative).
    //
    // In order to get around this, we force our channel count to go above 0 by
    // adding a large number >= 1 to it. This way no sender will see -1 unless
    // we are indeed blocking. This "extra lump" we took out of the channel
    // becomes our steal count (which will get re-factored into the count on the
    // next blocking recv)
    //
    // The return value of this method is whether there is data on this channel
    // to receive or not.
    fn abort_selection(&mut self, take_to_wake: bool) -> bool {
        // make sure steals + 1 makes the count go non-negative
        let steals = {
            let cnt = self.cnt.load(SeqCst);
            if cnt < 0 && cnt != DISCONNECTED {-cnt} else {0}
        };
        let prev = self.cnt.fetch_add(steals + 1, SeqCst);

        // If we were previously disconnected, then we know for sure that there
        // is no task into_wake, so just keep going
        if prev == DISCONNECTED {
            self.cnt.store(DISCONNECTED, SeqCst);
            return true;
        }

        // If the previous count was negative, then we just made things go
        // positive, hence we passed the -1 boundary and we're responsible for
        // removing the to_wake() field and trashing it.
        let cur = prev + steals + 1;
        assert!(cur >= 0);
        if prev <= -1 {
            if take_to_wake {
                self.to_wake.take_unwrap().trash();
            } else {
                assert!(self.to_wake.is_none());
            }
        }
        assert_eq!(self.steals, 0);
        self.steals = steals;
        // if we were previously positive, then there's surely data to receive
        return prev >= 0;
    }

    // Decrement the refere count on a channel. This is called whenever a Chan
    // is dropped and may end up waking up a receiver. It's the receiver's
    // responsibility on the other end to figure out that we've disconnected.
    unsafe fn drop_chan(&mut self) {
        match self.channels.fetch_sub(1, SeqCst) {
            1 => {
                match self.cnt.swap(DISCONNECTED, SeqCst) {
                    -1 => { self.to_wake.take_unwrap().wake(false); }
                    DISCONNECTED => {}
                    n => { assert!(n >= 0); }
                }
            }
            n if n > 1 => {},
            n => fail!("bad number of channels left {}", n),
        }
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        unsafe {
            assert!(self.to_wake.is_none());
            assert_eq!(self.channels.load(SeqCst), 0);
        }
    }
}

impl<T: Send> Chan<T> {
    /// Creates a new port/channel pair. All data send on the channel returned
    /// will become available on the port as well. See the documentation of
    /// `Port` and `Chan` to see what's possible with them.
    pub fn new() -> (Port<T>, Chan<T>) {
        // arbitrary 128 size cache -- this is just a max cache size, not a
        // maximum buffer size
        let (c, p) = spsc::queue(128, Packet::new());
        let c = SPSC(c);
        (Port { queue: c }, Chan { queue: p })
    }

    /// Sends a value along this channel to be received by the corresponding
    /// port.
    ///
    /// Rust channels are infinitely buffered so this method will never block.
    /// This method may trigger a rescheduling, however, in order to wake up a
    /// blocked receiver (if one is present). If no scheduling is desired, then
    /// the `send_deferred` guarantees that there will be no reschedulings.
    ///
    /// # Failure
    ///
    /// This function will fail if the other end of the channel has hung up.
    /// This means that if the corresponding port has fallen out of scope, this
    /// function will trigger a fail message saying that a message is being sent
    /// on a closed channel.
    ///
    /// Note that if this function does *not* fail, it does not mean that the
    /// data will be successfully received. All sends are placed into a queue,
    /// so it is possible for a send to succeed (the other end is alive), but
    /// then the other end could immediately disconnect.
    ///
    /// The purpose of this functionality is to propagate failure among tasks.
    /// If failure is not desired, then consider using the `try_send` method
    pub fn send(&self, t: T) {
        if !self.try_send(t) {
            fail!("sending on a closed channel");
        }
    }

    /// This function is equivalent in the semantics of `send`, but it
    /// guarantees that a rescheduling will never occur when this method is
    /// called.
    pub fn send_deferred(&self, t: T) {
        if !self.try_send_deferred(t) {
            fail!("sending on a closed channel");
        }
    }

    /// Attempts to send a value on this channel, returning whether it was
    /// successfully sent.
    ///
    /// A successful send occurs when it is determined that the other end of the
    /// channel has not hung up already. An unsuccessful send would be one where
    /// the corresponding port has already been deallocated. Note that a return
    /// value of `false` means that the data will never be received, but a
    /// return value of `true` does *not* mean that the data will be received.
    /// It is possible for the corresponding port to hang up immediately after
    /// this function returns `true`.
    ///
    /// Like `send`, this method will never block. If the failure of send cannot
    /// be tolerated, then this method should be used instead.
    pub fn try_send(&self, t: T) -> bool { self.try(t, true) }

    /// This function is equivalent in the semantics of `try_send`, but it
    /// guarantees that a rescheduling will never occur when this method is
    /// called.
    pub fn try_send_deferred(&self, t: T) -> bool { self.try(t, false) }

    fn try(&self, t: T, can_resched: bool) -> bool {
        unsafe {
            let this = cast::transmute_mut(self);
            this.queue.push(t);
            let packet = this.queue.packet();
            match (*packet).increment() {
                // As described above, -1 == wakeup
                -1 => { (*packet).to_wake.take_unwrap().wake(can_resched); true }
                // Also as above, SPSC queues must be >= -2
                -2 => true,
                // We succeeded if we sent data
                DISCONNECTED => this.queue.is_empty(),
                // In order to prevent starvation of other tasks in situations
                // where a task sends repeatedly without ever receiving, we
                // occassionally yield instead of doing a send immediately.
                // Only doing this if we're doing a rescheduling send, otherwise
                // the caller is expecting not to context switch.
                //
                // Note that we don't unconditionally attempt to yield because
                // the TLS overhead can be a bit much.
                n => {
                    if can_resched && n > 0 && n % RESCHED_FREQ == 0 {
                        imp::maybe_yield();
                    }
                    assert!(n >= 0); true
                }
            }
        }
    }
}

#[unsafe_destructor]
impl<T: Send> Drop for Chan<T> {
    fn drop(&mut self) {
        unsafe { (*self.queue.packet()).drop_chan(); }
    }
}

impl<T: Send> SharedChan<T> {
    /// Creates a new shared channel and port pair. The purpose of a shared
    /// channel is to be cloneable such that many tasks can send data at the
    /// same time. All data sent on any channel will become available on the
    /// provided port as well.
    pub fn new() -> (Port<T>, SharedChan<T>) {
        let (c, p) = mpsc::queue(Packet::new());
        let c = MPSC(c);
        (Port { queue: c }, SharedChan { queue: p })
    }

    /// Equivalent method to `send` on the `Chan` type (using the same
    /// semantics)
    pub fn send(&self, t: T) {
        if !self.try_send(t) {
            fail!("sending on a closed channel");
        }
    }

    /// This function is equivalent in the semantics of `send`, but it
    /// guarantees that a rescheduling will never occur when this method is
    /// called.
    pub fn send_deferred(&self, t: T) {
        if !self.try_send_deferred(t) {
            fail!("sending on a closed channel");
        }
    }

    /// Equivalent method to `try_send` on the `Chan` type (using the same
    /// semantics)
    pub fn try_send(&self, t: T) -> bool { self.try(t, true) }

    /// This function is equivalent in the semantics of `try_send`, but it
    /// guarantees that a rescheduling will never occur when this method is
    /// called.
    pub fn try_send_deferred(&self, t: T) -> bool { self.try(t, false) }

    fn try(&self, t: T, can_resched: bool) -> bool {
        unsafe {
            let this = cast::transmute_mut(self);
            this.queue.push(t);
            let packet = self.queue.packet();

            // Note that the multiple sender case is a little tricker
            // semantically than the single sender case. The logic for
            // incrementing is "add and if disconnected store disconnected".
            // This could end up leading some senders to believe that there
            // wasn't a disconnect if in fact there was a disconnect.
            //
            // The "disconnected" portion of a sender is already a bit weak, and
            // we at least guarantee that if N senders call send() that at least
            // one will always indicate that a disconnect was seen.
            //
            // Also note that the logic for returning whether this specific data
            // was sent is a little sketchy. The return value is already a very
            // loose idea of whether data was sent or not, so I believe that
            // this is OK.
            match (*packet).increment() {
                DISCONNECTED => this.queue.is_empty(),
                -1 => { (*packet).to_wake.take_unwrap().wake(can_resched); true }
                n => {
                    if can_resched && n > 0 && n % RESCHED_FREQ == 0 {
                        imp::maybe_yield();
                    }
                    true
                }
            }
        }
    }
}

impl<T: Send> Clone for SharedChan<T> {
    fn clone(&self) -> SharedChan<T> {
        unsafe { (*self.queue.packet()).channels.fetch_add(1, SeqCst); }
        SharedChan { queue: self.queue.clone() }
    }
}

#[unsafe_destructor]
impl<T: Send> Drop for SharedChan<T> {
    fn drop(&mut self) {
        unsafe { (*self.queue.packet()).drop_chan(); }
    }
}

impl<T: Send> Port<T> {
    /// Blocks waiting for a value on this port
    ///
    /// This function will block if necessary to wait for a corresponding send
    /// on the channel from its paired `Chan` structure. This port will be woken
    /// up when data is ready, and the data will be returned.
    ///
    /// # Failure
    ///
    /// Similar to channels, this method will trigger a task failure if the
    /// other end of the channel has hung up (been deallocated). The purpose of
    /// this is to propagate failure among tasks.
    ///
    /// If failure is not desired, then there are two options:
    ///
    /// * If blocking is still desired, the `recv_opt` method will return `None`
    ///   when the other end hangs up
    ///
    /// * If blocking is not desired, then the `try_recv` method will attempt to
    ///   peek at a value on this port.
    pub fn recv(&self) -> T {
        match self.recv_opt() {
            Some(t) => t,
            None => fail!("receiving on a closed channel"),
        }
    }

    /// Attempts to return a pending value on this port without blocking
    ///
    /// This method will never block the caller in order to wait for data to
    /// become available. Instead, this will always return immediately with a
    /// possible option of pending data on the channel.
    ///
    /// This is useful for a flavor of "optimistic check" before deciding to
    /// block on a port.
    ///
    /// This function cannot fail.
    pub fn try_recv(&self) -> Option<T> {
        // This is a "best effort" situation, so if a queue is inconsistent just
        // don't worry about it.
        let this = unsafe { cast::transmute_mut(self) };
        let ret = match this.queue {
            SPSC(ref mut queue) => queue.pop(),
            MPSC(ref mut queue) => match queue.pop() {
                mpsc::Data(t) => Some(t),
                mpsc::Empty | mpsc::Inconsistent => None,
            }
        };
        if ret.is_some() {
            unsafe { (*this.queue.packet()).steals += 1; }
        }
        return ret;
    }

    // Helper function for select, tests whether this port can receive without
    // blocking (obviously not an atomic decision).
    fn can_recv(&self) -> bool {
        unsafe {
            let packet = self.queue.packet();
            let cnt = (*packet).cnt.load(SeqCst);
            cnt == DISCONNECTED || cnt - (*packet).steals > 0
        }
    }

    /// Attempt to wait for a value on this port, but does not fail if the
    /// corresponding channel has hung up.
    ///
    /// This implementation of iterators for ports will always block if there is
    /// not data available on the port, but it will not fail in the case that
    /// the channel has been deallocated.
    ///
    /// In other words, this function has the same semantics as the `recv`
    /// method except for the failure aspect.
    ///
    /// If the channel has hung up, then `None` is returned. Otherwise `Some` of
    /// the value found on the port is returned.
    pub fn recv_opt(&self) -> Option<T> {
        // optimistic preflight check (scheduling is expensive)
        match self.try_recv() { None => {}, data => return data }

        let packet;
        let this;
        unsafe {
            this = cast::transmute_mut(self);
            packet = this.queue.packet();
            BlockingContext::one(&mut (*packet).data, |ctx, data| {
                ctx.block(data, &mut (*packet).to_wake, || (*packet).decrement())
            });
        }

        let data = match this.queue {
            SPSC(ref mut queue) => queue.pop(),
            MPSC(ref mut queue) => {
                match queue.pop() {
                    mpsc::Data(t) => Some(t),
                    mpsc::Empty => None,
                    // This is a bit of an interesting case. The channel is
                    // reported as having data available, but our pop() has
                    // failed due to the queue being in an inconsistent state.
                    // This means that there is some pusher somewhere which has
                    // yet to complete, but we are guaranteed that a pop will
                    // eventually succeed. In this case, we spin in a yield loop
                    // because the remote sender should finish their enqueue
                    // operation "very quickly".
                    //
                    // Avoiding this yield loop would require a different queue
                    // abstraction which provides the guarantee that after M
                    // pushes have succeeded, at least M pops will succeed. The
                    // current queues guarantee that if there are N active
                    // pushes, you can pop N times once all N have finished.
                    mpsc::Inconsistent => {
                        let data;
                        loop {
                            imp::yield_now();
                            match queue.pop() {
                                mpsc::Data(t) => { data = t; break }
                                mpsc::Empty => fail!("inconsistent => empty"),
                                mpsc::Inconsistent => {}
                            }
                        }
                        Some(data)
                    }
                }
            }
        };
        if data.is_none() &&
           unsafe { (*packet).cnt.load(SeqCst) } != DISCONNECTED {
            fail!("bug: woke up too soon");
        }
        return data;
    }

    /// Returns an iterator which will block waiting for messages, but never
    /// `fail!`. It will return `None` when the channel has hung up.
    pub fn iter<'a>(&'a self) -> PortIterator<'a, T> {
        PortIterator { port: self }
    }
}

impl<'a, T: Send> Iterator<T> for PortIterator<'a, T> {
    fn next(&mut self) -> Option<T> { self.port.recv_opt() }
}

#[unsafe_destructor]
impl<T: Send> Drop for Port<T> {
    fn drop(&mut self) {
        // All we need to do is store that we're disconnected. If the channel
        // half has already disconnected, then we'll just deallocate everything
        // when the shared packet is deallocated.
        unsafe {
            (*self.queue.packet()).cnt.store(DISCONNECTED, SeqCst);
        }
    }
}

#[cfg(test)]
mod test {
    use prelude::*;

    use task;
    use io::timer;
    use rt::thread::Thread;
    use unstable::run_in_bare_thread;
    use super::*;
    use rt::test::*;

    macro_rules! test (
        { fn $name:ident() $b:block $($a:attr)*} => (
            mod $name {
                #[allow(unused_imports)];

                use util;
                use super::super::*;
                use prelude::*;

                fn f() $b

                $($a)* #[test] fn uv() { f() }
                $($a)* #[test] fn native() {
                    use unstable::run_in_bare_thread;
                    run_in_bare_thread(f);
                }
            }
        )
    )

    test!(fn smoke() {
        let (p, c) = Chan::new();
        c.send(1);
        assert_eq!(p.recv(), 1);
    })

    test!(fn drop_full() {
        let (_p, c) = Chan::new();
        c.send(~1);
    })

    test!(fn drop_full_shared() {
        let (_p, c) = SharedChan::new();
        c.send(~1);
    })

    test!(fn smoke_shared() {
        let (p, c) = SharedChan::new();
        c.send(1);
        assert_eq!(p.recv(), 1);
        let c = c.clone();
        c.send(1);
        assert_eq!(p.recv(), 1);
    })

    #[test]
    fn smoke_threads() {
        let (p, c) = Chan::new();
        do task::spawn_sched(task::SingleThreaded) {
            c.send(1);
        }
        assert_eq!(p.recv(), 1);
    }

    #[test] #[should_fail]
    fn smoke_port_gone() {
        let (p, c) = Chan::new();
        drop(p);
        c.send(1);
    }

    #[test] #[should_fail]
    fn smoke_shared_port_gone() {
        let (p, c) = SharedChan::new();
        drop(p);
        c.send(1);
    }

    #[test] #[should_fail]
    fn smoke_shared_port_gone2() {
        let (p, c) = SharedChan::new();
        drop(p);
        let c2 = c.clone();
        drop(c);
        c2.send(1);
    }

    #[test] #[should_fail]
    fn port_gone_concurrent() {
        let (p, c) = Chan::new();
        do task::spawn_sched(task::SingleThreaded) {
            p.recv();
        }
        loop { c.send(1) }
    }

    #[test] #[should_fail]
    fn port_gone_concurrent_shared() {
        let (p, c) = SharedChan::new();
        let c1 = c.clone();
        do task::spawn_sched(task::SingleThreaded) {
            p.recv();
        }
        loop {
            c.send(1);
            c1.send(1);
        }
    }

    #[test] #[should_fail]
    fn smoke_chan_gone() {
        let (p, c) = Chan::<int>::new();
        drop(c);
        p.recv();
    }

    #[test] #[should_fail]
    fn smoke_chan_gone_shared() {
        let (p, c) = SharedChan::<()>::new();
        let c2 = c.clone();
        drop(c);
        drop(c2);
        p.recv();
    }

    #[test] #[should_fail]
    fn chan_gone_concurrent() {
        let (p, c) = Chan::new();
        do task::spawn_sched(task::SingleThreaded) {
            c.send(1);
            c.send(1);
        }
        loop { p.recv(); }
    }

    #[test]
    fn stress() {
        let (p, c) = Chan::new();
        do task::spawn_sched(task::SingleThreaded) {
            for _ in range(0, 10000) { c.send(1); }
        }
        for _ in range(0, 10000) {
            assert_eq!(p.recv(), 1);
        }
    }

    #[test]
    fn stress_shared() {
        static AMT: uint = 10000;
        static NTHREADS: uint = 8;
        let (p, c) = SharedChan::<int>::new();
        let (p1, c1) = Chan::new();

        do spawn {
            for _ in range(0, AMT * NTHREADS) {
                assert_eq!(p.recv(), 1);
            }
            assert_eq!(p.try_recv(), None);
            c1.send(());
        }

        for _ in range(0, NTHREADS) {
            let c = c.clone();
            do task::spawn_sched(task::SingleThreaded) {
                for _ in range(0, AMT) { c.send(1); }
            }
        }
        p1.recv();

    }

    #[test]
    fn send_from_outside_runtime() {
        let (p, c) = Chan::<int>::new();
        let (p1, c1) = Chan::new();
        do spawn {
            c1.send(());
            for _ in range(0, 40) {
                assert_eq!(p.recv(), 1);
            }
        }
        p1.recv();
        let t = do Thread::start {
            for _ in range(0, 40) {
                c.send(1);
            }
        };
        t.join();
    }

    #[test]
    fn recv_from_outside_runtime() {
        let (p, c) = Chan::<int>::new();
        let t = do Thread::start {
            for _ in range(0, 40) {
                assert_eq!(p.recv(), 1);
            }
        };
        for _ in range(0, 40) {
            c.send(1);
        }
        t.join();
    }

    #[test]
    fn no_runtime() {
        let (p1, c1) = Chan::<int>::new();
        let (p2, c2) = Chan::<int>::new();
        let t1 = do Thread::start {
            assert_eq!(p1.recv(), 1);
            c2.send(2);
        };
        let t2 = do Thread::start {
            c1.send(1);
            assert_eq!(p2.recv(), 2);
        };
        t1.join();
        t2.join();
    }

    test!(fn select_smoke() {
        let (p1, c1) = Chan::<int>::new();
        let (p2, _c2) = Chan::<int>::new();
        c1.send(1);

        let ports = [&p1, &p2];
        assert_eq!(select(ports), 0);
        assert_eq!(select(ports), 0);
        assert_eq!(select(ports), 0);
    })

    test!(fn select_closed() {
        let (p1, _c1) = Chan::<int>::new();
        let (p2, c2) = Chan::<int>::new();
        drop(c2);

        let ports = [&p1, &p2];
        assert_eq!(select(ports), 1);
    })

    #[test]
    fn select_unblocks() {
        let (p1, c1) = Chan::<int>::new();
        let (p2, _c2) = Chan::<int>::new();
        let (p3, c3) = Chan::<int>::new();

        do spawn {
            timer::sleep(3);
            c1.send(1);
            p3.recv();
            timer::sleep(3);
        }

        {
            let ports = [&p1, &p2];
            assert_eq!(select(ports), 0);
            assert_eq!(select(ports), 0);
        }
        assert_eq!(p1.try_recv(), Some(1));
        c3.send(1);
        {
            let ports = [&p1, &p2];
            assert_eq!(select(ports), 0);
        }
        assert_eq!(p1.try_recv(), None);
    }

    #[test]
    fn select_both_ready() {
        let (p1, c1) = Chan::<int>::new();
        let (p2, c2) = Chan::<int>::new();

        do spawn {
            timer::sleep(3);
            c1.send(1);
            c2.send(1);
        }

        {
            let ports = [&p1, &p2];
            assert_eq!(select(ports), 0);
            assert_eq!(select(ports), 0);
        }
        p1.recv();
        {
            let ports = [&p2];
            assert_eq!(select(ports), 0);
        }
    }

    #[test]
    fn select_stress() {
        static AMT: int = 10000;
        let (p1, c1) = Chan::<int>::new();
        let (p2, c2) = Chan::<int>::new();
        let (p3, c3) = Chan::<()>::new();

        do spawn {
            for i in range(0, AMT) {
                if i % 2 == 0 {
                    c1.send(i);
                } else {
                    c2.send(i);
                }
                p3.recv();
            }
        }

        let ports = [&p1, &p2];
        for i in range(0, AMT) {
            assert!(select(ports) == (i % 2) as uint,
                    "fail on {}", i);
            assert_eq!(ports[i % 2].try_recv(), Some(i));
            c3.send(());
        }
    }

    #[test]
    fn select_stress_native() {
        static AMT: int = 10000;
        do run_in_bare_thread {
            let (p1, c1) = Chan::<int>::new();
            let (p2, c2) = Chan::<int>::new();
            let (p3, c3) = Chan::<()>::new();

            let t = do Thread::start {
                for i in range(0, AMT) {
                    if i % 2 == 0 {
                        c1.send(i);
                    } else {
                        c2.send(i);
                    }
                    p3.recv();
                }
            };

            let ports = [&p1, &p2];
            for i in range(0, AMT) {
                assert!(select(ports) == (i % 2) as uint,
                        "fail on {}", i);
                assert_eq!(ports[i % 2].try_recv(), Some(i));
                c3.send(());
            }
            t.join();
        }
    }

    #[test]
    fn select_native_both_ready() {
        do run_in_bare_thread {
            let (p1, c1) = Chan::<int>::new();
            let (p2, c2) = Chan::<int>::new();

            let t = do Thread::start {
                c1.send(1);
                c2.send(1);
            };

            {
                let ports = [&p1, &p2];
                assert_eq!(select(ports), 0);
                assert_eq!(select(ports), 0);
            }
            p1.recv();
            {
                let ports = [&p2];
                assert_eq!(select(ports), 0);
            }
            t.join();
        }
    }

    #[test]
    fn oneshot_single_thread_close_port_first() {
        // Simple test of closing without sending
        do run_in_newsched_task {
            let (port, _chan) = Chan::<int>::new();
            { let _p = port; }
        }
    }

    #[test]
    fn oneshot_single_thread_close_chan_first() {
        // Simple test of closing without sending
        do run_in_newsched_task {
            let (_port, chan) = Chan::<int>::new();
            { let _c = chan; }
        }
    }

    #[test] #[should_fail]
    fn oneshot_single_thread_send_port_close() {
        // Testing that the sender cleans up the payload if receiver is closed
        let (port, chan) = Chan::<~int>::new();
        { let _p = port; }
        chan.send(~0);
    }

    #[test]
    fn oneshot_single_thread_recv_chan_close() {
        // Receiving on a closed chan will fail
        do run_in_newsched_task {
            let res = do spawntask_try {
                let (port, chan) = Chan::<~int>::new();
                { let _c = chan; }
                port.recv();
            };
            // What is our res?
            assert!(res.is_err());
        }
    }

    #[test]
    fn oneshot_single_thread_send_then_recv() {
        do run_in_newsched_task {
            let (port, chan) = Chan::<~int>::new();
            chan.send(~10);
            assert!(port.recv() == ~10);
        }
    }

    #[test]
    fn oneshot_single_thread_try_send_open() {
        do run_in_newsched_task {
            let (port, chan) = Chan::<int>::new();
            assert!(chan.try_send(10));
            assert!(port.recv() == 10);
        }
    }

    #[test]
    fn oneshot_single_thread_try_send_closed() {
        do run_in_newsched_task {
            let (port, chan) = Chan::<int>::new();
            { let _p = port; }
            assert!(!chan.try_send(10));
        }
    }

    #[test]
    fn oneshot_single_thread_try_recv_open() {
        do run_in_newsched_task {
            let (port, chan) = Chan::<int>::new();
            chan.send(10);
            assert!(port.try_recv() == Some(10));
        }
    }

    #[test]
    fn oneshot_single_thread_try_recv_closed() {
        do run_in_newsched_task {
            let (port, chan) = Chan::<int>::new();
            { let _c = chan; }
            assert!(port.recv_opt() == None);
        }
    }

    #[test]
    fn oneshot_single_thread_peek_data() {
        do run_in_newsched_task {
            let (port, chan) = Chan::<int>::new();
            assert!(port.try_recv().is_none());
            chan.send(10);
            assert!(port.try_recv().is_some());
        }
    }

    #[test]
    fn oneshot_single_thread_peek_close() {
        do run_in_newsched_task {
            let (port, chan) = Chan::<int>::new();
            { let _c = chan; }
            assert!(port.try_recv().is_none());
            assert!(port.try_recv().is_none());
        }
    }

    #[test]
    fn oneshot_single_thread_peek_open() {
        do run_in_newsched_task {
            let (port, _) = Chan::<int>::new();
            assert!(port.try_recv().is_none());
        }
    }

    #[test]
    fn oneshot_multi_task_recv_then_send() {
        do run_in_newsched_task {
            let (port, chan) = Chan::<~int>::new();
            do spawntask {
                assert!(port.recv() == ~10);
            }

            chan.send(~10);
        }
    }

    #[test]
    fn oneshot_multi_task_recv_then_close() {
        do run_in_newsched_task {
            let (port, chan) = Chan::<~int>::new();
            do spawntask_later {
                let _chan = chan;
            }
            let res = do spawntask_try {
                assert!(port.recv() == ~10);
            };
            assert!(res.is_err());
        }
    }

    #[test]
    fn oneshot_multi_thread_close_stress() {
        stress_factor().times(|| {
            do run_in_newsched_task {
                let (port, chan) = Chan::<int>::new();
                let thread = do spawntask_thread {
                    let _p = port;
                };
                let _chan = chan;
                thread.join();
            }
        })
    }

    #[test]
    fn oneshot_multi_thread_send_close_stress() {
        stress_factor().times(|| {
            let (port, chan) = Chan::<int>::new();
            do spawn {
                let _p = port;
            }
            do task::try {
                chan.send(1);
            };
        })
    }

    #[test]
    fn oneshot_multi_thread_recv_close_stress() {
        stress_factor().times(|| {
            let (port, chan) = Chan::<int>::new();
            do spawn {
                let port = port;
                let res = do task::try {
                    port.recv();
                };
                assert!(res.is_err());
            };
            do spawn {
                let chan = chan;
                do spawn {
                    let _chan = chan;
                }
            };
        })
    }

    #[test]
    fn oneshot_multi_thread_send_recv_stress() {
        stress_factor().times(|| {
            let (port, chan) = Chan::<~int>::new();
            do spawn {
                chan.send(~10);
            }
            do spawn {
                assert!(port.recv() == ~10);
            }
        })
    }

    #[test]
    fn stream_send_recv_stress() {
        stress_factor().times(|| {
            let (port, chan) = Chan::<~int>::new();

            send(chan, 0);
            recv(port, 0);

            fn send(chan: Chan<~int>, i: int) {
                if i == 10 { return }

                do spawntask_random {
                    chan.send(~i);
                    send(chan, i + 1);
                }
            }

            fn recv(port: Port<~int>, i: int) {
                if i == 10 { return }

                do spawntask_random {
                    assert!(port.recv() == ~i);
                    recv(port, i + 1);
                };
            }
        })
    }

    #[test]
    fn recv_a_lot() {
        // Regression test that we don't run out of stack in scheduler context
        do run_in_newsched_task {
            let (port, chan) = Chan::new();
            10000.times(|| { chan.send(()) });
            10000.times(|| { port.recv() });
        }
    }

    #[test]
    fn shared_chan_stress() {
        do run_in_mt_newsched_task {
            let (port, chan) = SharedChan::new();
            let total = stress_factor() + 100;
            total.times(|| {
                let chan_clone = chan.clone();
                do spawntask_random {
                    chan_clone.send(());
                }
            });

            total.times(|| {
                port.recv();
            });
        }
    }

    #[test]
    fn test_nested_recv_iter() {
        let (port, chan) = Chan::<int>::new();
        let (total_port, total_chan) = Chan::<int>::new();

        do spawn {
            let mut acc = 0;
            for x in port.iter() {
                acc += x;
            }
            total_chan.send(acc);
        }

        chan.send(3);
        chan.send(1);
        chan.send(2);
        drop(chan);
        assert_eq!(total_port.recv(), 6);
    }

    #[test]
    fn test_recv_iter_break() {
        let (port, chan) = Chan::<int>::new();
        let (count_port, count_chan) = Chan::<int>::new();

        do spawn {
            let mut count = 0;
            for x in port.iter() {
                if count >= 3 {
                    break;
                } else {
                    count += x;
                }
            }
            count_chan.send(count);
        }

        chan.send(2);
        chan.send(2);
        chan.send(2);
        chan.try_send(2);
        drop(chan);
        assert_eq!(count_port.recv(), 4);
    }
}
