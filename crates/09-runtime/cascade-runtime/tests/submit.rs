//! T5.1 (FR-35, §6.1): the `submit_to` primitive.
//!
//! Each test runs the CALLER on its own executor and the TARGET on a
//! spawned executor pinned to a distinct core — real reactors, real
//! crossing. The plan's test bullets map one-to-one: Send-closure state
//! runs on target (thread-id assert); `!Send` state created inside the
//! future (an `Rc` made on the target) works; the result returns to the
//! caller; jobs queue while the target is busy; and the `!Send`
//! *closure* is rejected — at compile time, by `tests/compile_fail.rs`
//! (trybuild), not by a commented-out line.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread::ThreadId,
    time::Duration,
};

use cascade_runtime::{pin, submit};
use glommio::{timer::sleep, ExecutorJoinHandle, LocalExecutorBuilder, Placement};
use submit::SubmitError;

/// Pin-heavy fixtures must not overlap (deployment.rs discipline).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Two distinct allowed cores: the target pins to the second so the
/// caller's ring and the target's ring are different reactors for sure.
fn cores() -> (usize, usize) {
    let allowed = pin::current_affinity();
    assert!(allowed.len() >= 2, "needs 2 CPUs");
    (allowed[0] as usize, allowed[1] as usize)
}

/// Boots a target ring on `core` running `submit_loop` (draining
/// forever), and returns the unconnected producer, the stats Arc the
/// loop shares with the eventual handle, the target thread's id
/// (recorded by the ring's own thread), and its join handle.
fn spawn_target(
    core: usize,
) -> (
    submit::SubmitInit,
    Arc<submit::SubmitStats>,
    Arc<std::sync::OnceLock<ThreadId>>,
    ExecutorJoinHandle<()>,
) {
    let (init, rx) = submit::channel(8);
    let stats = init.stats();
    let target_id = Arc::new(std::sync::OnceLock::new());
    let id_slot = Arc::clone(&target_id);
    let loop_stats = Arc::clone(&stats);
    let join = LocalExecutorBuilder::new(Placement::Fixed(core))
        .name("submit-target")
        .spawn(move || {
            id_slot.set(std::thread::current().id()).ok();
            async move {
                let recv = rx.connect().await;
                submit::submit_loop(recv, loop_stats).await;
            }
        })
        .expect("target executor spawns");
    (init, stats, target_id, join)
}

fn caller_executor(name: &str, core: usize) -> glommio::LocalExecutor {
    LocalExecutorBuilder::new(Placement::Fixed(core))
        .name(name)
        .make()
        .expect("caller executor")
}

/// The plan's first three test bullets in one real crossing: a Send
/// closure (captured `std::sync::mpsc::Sender`), a `!Send` `Rc`
/// constructed inside the future on the target, and the result coming
/// back — with a thread-id assert proving the work ran on the target
/// ring, not the caller's.
#[test]
fn send_closure_runs_on_target_and_rc_made_inside_works() {
    let _serial = serial();
    let (caller_core, target_core) = cores();
    let (init, _stats, target_id, _join) = spawn_target(target_core);
    let ex = caller_executor("submit-caller", caller_core);

    let (value, caller_thread, job_thread) = ex.run(async move {
            let handle = init.connect().await;
            let caller_thread = std::thread::current().id();

            // Send captured state: a plain mpsc sender crosses; the Rc
            // is made INSIDE the future, on the target — legal per
            // FR-35/R14 and impossible for a pre-built future.
            let (tx, rx) = std::sync::mpsc::channel::<ThreadId>();
            let submitted = handle.submit_to(move || {
                let tx = tx;
                async move {
                    let local_only = std::rc::Rc::new(41usize);
                    let _ = tx.send(std::thread::current().id());
                    *local_only + 1
                }
            });
            let value = submitted.await.expect("result returns to caller");
            (value, caller_thread, rx.recv().expect("job reported its thread"))
        });

    assert_eq!(value, 42, "the future's output crosses back");
    let target = target_id.get().expect("target recorded its thread id");
    assert_eq!(job_thread, *target, "the closure ran on the target ring");
    assert_ne!(
        job_thread, caller_thread,
        "the work did not run on the caller's reactor"
    );
}

/// Queueing when the target is busy: the first job gates on a flag
/// (yielding while it waits — never blocking the ring), four more queue
/// behind it, all start in submission order (the ring's task queue is
/// FIFO), and every one ran on the target.
#[test]
fn jobs_queue_while_target_is_busy() {
    let _serial = serial();
    let (caller_core, target_core) = cores();
    let (init, _stats, target_id, _join) = spawn_target(target_core);
    let ex = caller_executor("submit-caller", caller_core);

    ex.run(async move {
        let handle = init.connect().await;
        let gate = Arc::new(AtomicBool::new(false));
        let start_seq = Arc::new(AtomicU64::new(0));
        let target = target_id.get().expect("target thread id").to_owned();

        let mut results = Vec::new();
        for i in 0..5u64 {
            let gate = Arc::clone(&gate);
            let seq = Arc::clone(&start_seq);
            let fut = handle.submit_to(move || {
                let gate = gate;
                let seq = seq;
                async move {
                    let order = seq.fetch_add(1, Ordering::SeqCst) + 1;
                    if i == 0 {
                        // Hold the ring BUSY for the followers: yield,
                        // never block — a spinning job without yields
                        // would wedge the cooperative ring.
                        while !gate.load(Ordering::SeqCst) {
                            sleep(Duration::from_millis(1)).await;
                        }
                    }
                    (i, order, std::thread::current().id())
                }
            });
            results.push(fut);
        }

        // Everything queued while the gate held; release and collect.
        gate.store(true, Ordering::SeqCst);
        let mut seen = Vec::new();
        for (i, fut) in results.into_iter().enumerate() {
            let (job, order, tid) = fut.await.expect("queued job completes");
            assert_eq!(job, i as u64, "results return per job");
            assert_eq!(tid, target, "every job ran on the target ring");
            seen.push((job, order));
        }
        seen.sort();
        let orders: Vec<u64> = seen.iter().map(|(_, o)| *o).collect();
        assert_eq!(
            orders,
            vec![1, 2, 3, 4, 5],
            "jobs started in submission order while busy"
        );

        let stats = handle.stats();
        assert_eq!(stats.submitted.load(Ordering::Relaxed), 5);
        assert_eq!(stats.executed.load(Ordering::Relaxed), 5);
        assert_eq!(stats.refused.load(Ordering::Relaxed), 0);
    });
}

/// Backpressure: a connected-but-undrained queue of capacity 1 takes
/// one job and refuses the next with the counted `Saturated` — the
/// caller never blocks. (Deterministic because nothing drains: the
/// receiver's ring parks after connecting — and its connect must run
/// on a spawned thread CONCURRENT with the caller's producer connect;
/// sequenced on one thread, whichever side goes first spins forever
/// waiting for a rendezvous partner that is not connecting yet — the
/// T4.5 rule, live again.)
#[test]
fn full_queue_refuses_without_blocking() {
    let _serial = serial();
    let (caller_core, target_core) = cores();
    let (init, rx) = submit::channel(1);
    let stats = init.stats();

    // The receiver connects and then parks forever: capacity 1,
    // nothing ever drains. Dropping the join handle detaches the
    // parked (0% CPU) ring; the process exit reaps it.
    let parked = LocalExecutorBuilder::new(Placement::Fixed(target_core))
        .name("submit-parked")
        .spawn(move || async move {
            let cr = rx.connect().await;
            std::future::pending::<()>().await;
            let _ = cr; // keep the receiver alive for the queue's life
        })
        .expect("parked executor spawns");

    let ex = caller_executor("submit-caller", caller_core);
    let unresolved_first = ex.run(async move {
        let handle = init.connect().await;
        let gate = Arc::new(AtomicBool::new(false));
        let first = handle.submit_to(move || {
            let gate = gate;
            async move {
                while !gate.load(Ordering::SeqCst) {
                    sleep(Duration::from_millis(1)).await;
                }
                1u64
            }
        });
        let second = handle.submit_to(|| async move { 2u64 });
        assert_eq!(
            second.await,
            Err(SubmitError::Saturated),
            "a full queue refuses, counted"
        );
        assert_eq!(stats.refused.load(Ordering::Relaxed), 1);
        assert_eq!(stats.submitted.load(Ordering::Relaxed), 2);
        first // never drained here; dropped with the queue below
    });
    drop(unresolved_first);
    drop(parked);
}

/// The lost-result contract: a queued job whose queue is torn down
/// under it resolves the caller with `Lost` instead of hanging. The
/// teardown is sequenced by flags, not sleeps: the target drops its
/// receiver once the submit has happened; the caller drops its
/// producer handle before awaiting, so BOTH endpoints are gone and the
/// queued job (holding the oneshot sender) drops with the buffer.
#[test]
fn torn_down_queue_resolves_lost_not_hang() {
    let _serial = serial();
    let (caller_core, target_core) = cores();
    let (init, rx) = submit::channel(8);

    let submitted = Arc::new(AtomicBool::new(false));
    // Target: connect, then drop the receiver as soon as the submit
    // flag fires (poll at 1 ms — sequencing, not timing-guessing).
    let flag = Arc::clone(&submitted);
    let target_join = LocalExecutorBuilder::new(Placement::Fixed(target_core))
        .name("submit-victim-2")
        .spawn(move || {
            async move {
                let cr = rx.connect().await;
                while !flag.load(Ordering::SeqCst) {
                    sleep(Duration::from_millis(1)).await;
                }
                drop(cr); // consumer endpoint gone
            }
        })
        .expect("victim executor spawns");

    let ex = caller_executor("submit-caller", caller_core);
    let result = ex.run(async move {
        let handle = init.connect().await;
        let fut = handle.submit_to(|| async move { 9u64 });
        submitted.store(true, Ordering::SeqCst);
        drop(handle); // producer endpoint gone — both halves now drop
        fut.await
    });

    target_join.join().expect("victim ring ends");
    match result {
        Err(SubmitError::Lost) => {}
        Err(SubmitError::Saturated) => {
            panic!("queue was empty; saturation is not the lost path")
        }
        Ok(9) => panic!("a torn-down queue cannot deliver"),
        other => panic!("unexpected outcome: {other:?}"),
    }
}

/// The plan's review bullet, pinned: a PANICKING job resolves the
/// caller with `Lost` — never a hang — and, per glommio 0.9's task
/// machinery (drop_future during unwind cancels the oneshot), the
/// panic then propagates through the executor loop and kills the
/// whole target ring. The caller is on a different ring and survives.
#[test]
fn panicking_job_resolves_lost_and_the_ring_dies() {
    let _serial = serial();
    let (caller_core, target_core) = cores();
    let (init, rx) = submit::channel(8);
    let loop_stats = init.stats();

    let target_join = LocalExecutorBuilder::new(Placement::Fixed(target_core))
        .name("submit-doomed")
        .spawn(move || async move {
            let recv = rx.connect().await;
            submit::submit_loop(recv, loop_stats).await;
        })
        .expect("doomed executor spawns");

    let ex = caller_executor("submit-caller", caller_core);
    let result = ex.run(async move {
        let handle = init.connect().await;
        let f = handle.submit_to(|| async move {
            panic!("boom: the job dies");
            #[allow(unreachable_code)]
            1u64
        });
        f.await
    });

    assert_eq!(
        result,
        Err(SubmitError::Lost),
        "a panicking job resolves Lost, not a hang"
    );
    // The blast radius is the ring itself: the executor loop panicked
    // too (no catch_unwind in glommio's run loop) — the join reports
    // it, which is the documented T5.4 hazard.
    assert!(
        target_join.join().is_err(),
        "the target ring died with the panicking job"
    );
}
