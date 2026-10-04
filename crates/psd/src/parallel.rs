//! Ordered codec jobs with a bounded window of outstanding results.

use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use psd_core::Result;

const SCRATCH_BUDGET: usize = 128 * 1024 * 1024;
const PARALLEL_MIN_BYTES: usize = 64 * 1024;

pub(crate) fn for_each_ordered<J: Send, O: Send>(
    jobs: Vec<(J, usize)>,
    compute: impl Fn(J) -> Result<O> + Sync,
    consume: impl FnMut(O) -> Result<()>,
) -> Result<()> {
    for_each_ordered_with_budget(jobs, SCRATCH_BUDGET, compute, consume)
}

/// Credits remain reserved until an output has been consumed, so completed
/// results cannot accumulate behind a slow earlier job without a bound.
pub(crate) fn for_each_ordered_with_budget<J: Send, O: Send>(
    jobs: Vec<(J, usize)>,
    budget: usize,
    compute: impl Fn(J) -> Result<O> + Sync,
    mut consume: impl FnMut(O) -> Result<()>,
) -> Result<()> {
    let total = jobs
        .iter()
        .fold(0usize, |sum, (_, bytes)| sum.saturating_add(*bytes));
    if jobs.len() < 2
        || budget == 0
        || total < PARALLEL_MIN_BYTES
        || rayon::current_num_threads() == 1
    {
        for (job, _) in jobs {
            consume(compute(job)?)?;
        }
        return Ok(());
    }
    let threads = rayon::current_num_threads();
    let compute = &compute;
    let cancel = AtomicBool::new(false);
    rayon::in_place_scope(|scope| {
        struct Cancel<'a>(&'a AtomicBool);
        impl Drop for Cancel<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _cancel_on_exit = Cancel(&cancel);
        let (sender, receiver) = mpsc::channel();
        let mut jobs = jobs.into_iter().enumerate().peekable();
        let mut reserved = 0usize;
        let mut outstanding = 0usize;
        let mut next = 0usize;
        let mut completed = BTreeMap::new();
        loop {
            while let Some((_, (_, cost))) = jobs.peek() {
                if outstanding >= threads
                    || (outstanding > 0 && reserved.saturating_add(*cost) > budget)
                {
                    break;
                }
                let (index, (job, cost)) = jobs.next().unwrap();
                reserved = reserved.saturating_add(cost);
                outstanding += 1;
                let sender = sender.clone();
                let cancel = &cancel;
                scope.spawn(move |_| {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    // Catch worker panics so the coordinator never waits for a
                    // missing result. Re-raise them after earlier results drain.
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| compute(job)));
                    let _ = sender.send((index, cost, result));
                });
            }
            if outstanding == 0 {
                break;
            }
            while !completed.contains_key(&next) {
                let result = if rayon::current_thread_index().is_some() {
                    loop {
                        match receiver.try_recv() {
                            Ok(result) => break result,
                            Err(mpsc::TryRecvError::Empty) => {
                                rayon::yield_now();
                                std::thread::yield_now();
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                panic!("codec result channel disconnected")
                            }
                        }
                    }
                } else {
                    receiver.recv().expect("scoped codec worker")
                };
                let (index, cost, result) = result;
                completed.insert(index, (cost, result));
            }
            let (cost, result) = completed.remove(&next).unwrap();
            let value = match result {
                Ok(result) => result?,
                Err(panic) => std::panic::resume_unwind(panic),
            };
            consume(value)?;
            reserved = reserved.saturating_sub(cost);
            outstanding -= 1;
            next += 1;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_refills_without_waiting_for_a_slow_later_job() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let mut order = Vec::new();
        pool.in_place_scope(|_| {
            for_each_ordered_with_budget(
                vec![(0, 65536), (1, 65536), (2, 65536)],
                131072,
                |index| {
                    if index == 1 {
                        let (lock, ready) = &*gate;
                        let guard = lock.lock().unwrap();
                        let (guard, _) = ready
                            .wait_timeout_while(guard, std::time::Duration::from_secs(5), |ready| {
                                !*ready
                            })
                            .unwrap();
                        assert!(
                            *guard,
                            "the third job must start before the second finishes"
                        );
                    } else if index == 2 {
                        let (lock, ready) = &*gate;
                        *lock.lock().unwrap() = true;
                        ready.notify_all();
                    }
                    Ok(index)
                },
                |index| {
                    order.push(index);
                    Ok(())
                },
            )
            .unwrap();
        });
        assert_eq!(order, [0, 1, 2]);
    }

    #[test]
    fn nested_single_thread_and_worker_panics_do_not_lose_results() {
        for threads in [1, 2] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                let output = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                for_each_ordered(vec![(0, 65536), (1, 65536)], Ok, |value| {
                    output.borrow_mut().push(value);
                    Ok(())
                })
                .unwrap();
                assert_eq!(*output.borrow(), [0, 1]);
                let panic = std::panic::catch_unwind(|| {
                    for_each_ordered(
                        vec![(0, 65536), (1, 65536)],
                        |index| {
                            if index == 1 {
                                panic!("intentional codec panic");
                            }
                            Ok(index)
                        },
                        |_| Ok(()),
                    )
                });
                assert!(panic.is_err());
            });
        }
    }
}
