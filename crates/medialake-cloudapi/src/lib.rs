//! A typed layer over the Windows Cloud Files API (cfapi.h): sync roots, placeholders, hydration
//! callbacks and pin state.
//!
//! The crate has two halves so that most of it can be built and tested anywhere:
//!
//! * the **platform-neutral** half (this file, [`types`], [`plan`]): the identity blob stored in every
//!   placeholder, the mapping from callback paths to sync paths, the 4 KiB transfer planner, and the
//!   [`CloudApi`] / [`Provider`] traits that the frontend in `medialake-cloud` is written against;
//! * the **Windows** half ([`win`], `cfg(windows)`): the implementation over the `windows` crate. It
//!   cannot run off Windows, but it type-checks from any host:
//!   `cargo check -p medialake-cloudapi --target x86_64-pc-windows-msvc --all-targets`.
//!
//! See `docs/w0-cloud-files.md` for what is assumed about the platform and how to confirm it.

pub mod plan;
pub mod types;

#[cfg(windows)]
pub mod win;

pub use plan::{ALIGN, plan_chunks, rel_from_normalized};
pub use types::*;
