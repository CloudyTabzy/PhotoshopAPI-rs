//! Ordered codec jobs with a bounded amount of concurrent scratch memory.

use rayon::prelude::*;

use psd_core::Result;

// Jobs estimate extra codec workspace, not the document's canonical pixels or
// encoded output. A job larger than the budget runs alone. Small documents
// avoid initializing the Rayon pool at all.
const SCRATCH_BUDGET: usize = 128 * 1024 * 1024;
const PARALLEL_MIN_BYTES: usize = 64 * 1024;

pub(crate) fn for_each_ordered<J: Send, O: Send>(
    jobs: Vec<(J, usize)>,
    compute: impl Fn(J) -> Result<O> + Sync,
    mut consume: impl FnMut(O) -> Result<()>,
) -> Result<()> {
    let total = jobs
        .iter()
        .fold(0usize, |sum, (_, bytes)| sum.saturating_add(*bytes));
    if jobs.len() < 2 || total < PARALLEL_MIN_BYTES {
        for (job, _) in jobs {
            consume(compute(job)?)?;
        }
        return Ok(());
    }

    let threads = rayon::current_num_threads();
    let mut jobs = jobs.into_iter().peekable();
    while jobs.peek().is_some() {
        let mut batch = Vec::with_capacity(threads);
        let mut bytes = 0usize;
        while let Some((_, cost)) = jobs.peek() {
            let next = bytes.saturating_add(*cost);
            if !batch.is_empty() && (next > SCRATCH_BUDGET || batch.len() >= threads) {
                break;
            }
            bytes = next;
            batch.push(jobs.next().expect("peeked job").0);
        }
        // Collect Results before propagating errors: indexed collection keeps
        // file order and therefore reports the same first error on every run.
        let results: Vec<_> = if batch.len() == 1 {
            batch.into_iter().map(&compute).collect()
        } else {
            batch.into_par_iter().map(&compute).collect()
        };
        for result in results {
            consume(result?)?;
        }
    }
    Ok(())
}
