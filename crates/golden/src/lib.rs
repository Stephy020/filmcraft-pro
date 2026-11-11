//! Golden-image tests for EpsiCut's renderer (test-only crate; the library is empty).
//!
//! `tests/golden.rs` builds small procedural projects (no media files), renders one frame of each
//! through the CPU reference compositor (`epsicut-render`) and compares it with a committed PNG
//! in `goldens/` using `epsicut_testkit::golden`. The same scenes are composited on the GPU
//! (`epsicut-gpu`) when an adapter is available and compared with the CPU result.
//!
//! Regenerate references with `EPSICUT_BLESS=1 cargo test -p epsicut-golden`.
