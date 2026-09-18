//! Hard resource limits shared by the tail/grep/stream engines.
//!
//! The sidecar has a hard RSS budget (< 15 MB). File sizes are unbounded, so
//! every engine caps how much of a file it is willing to assemble in memory.

/// Maximum line length in bytes. Longer lines are skipped entirely by grep
/// and the live stream (never assembled, never matched), and the tail window
/// is byte-capped separately. This keeps RSS bounded on pathological inputs —
/// a rotated `.gz` sitting in the log dir, or a multi-GiB line with no `\n`.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Maximum byte size of a tail window. A file with fewer newlines than
/// requested is cut to this window (`tail -c` semantics); the first line of
/// a cut window may be partial.
pub const MAX_TAIL_WINDOW_BYTES: u64 = 4 * 1024 * 1024;
