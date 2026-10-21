//! The Tokio runtime for the binary's async parts outside `views` (the HAPI
//! database export and the refresh database load): one current-thread
//! runtime per thread, built on first use and kept for the run. `views`
//! runs on a multi-thread runtime of its own, since it uses
//! `block_in_place`; view evaluation keeps its runtimes in sof.

use std::cell::OnceCell;
use std::future::Future;

use tokio::runtime::Runtime;

thread_local! {
    static RUNTIME: OnceCell<Runtime> = const { OnceCell::new() };
}

/// Run `future` to completion on this thread's runtime. Not for use inside
/// an async runtime. An error is a runtime that could not be built.
pub fn block_on<F: Future>(future: F) -> std::io::Result<F::Output> {
    RUNTIME.with(|cell| {
        if cell.get().is_none() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let _ = cell.set(rt);
        }
        Ok(cell.get().expect("just set").block_on(future))
    })
}
