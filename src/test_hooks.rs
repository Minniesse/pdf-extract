//! Counters a caller's tests read to prove what this crate did on the current thread. Always
//! compiled; they only count.
use std::cell::Cell;

thread_local! {
    static STREAMS: Cell<u64> = const { Cell::new(0) };
    static FILTERED: Cell<u64> = const { Cell::new(0) };
}

pub(crate) fn count_stream() {
    STREAMS.with(|count| count.set(count.get().saturating_add(1)));
}

pub(crate) fn count_filtered() {
    FILTERED.with(|count| count.set(count.get().saturating_add(1)));
}

/// Streams whose bytes this thread read.
#[doc(hidden)]
pub fn streams_read() -> u64 {
    STREAMS.with(Cell::get)
}

/// Streams with a `/Filter` that reached `get_contents` on this thread (each one panicked).
#[doc(hidden)]
pub fn filtered_get_contents() -> u64 {
    FILTERED.with(Cell::get)
}

/// Sets this thread's counters to zero.
#[doc(hidden)]
pub fn reset() {
    STREAMS.with(|count| count.set(0));
    FILTERED.with(|count| count.set(0));
}
