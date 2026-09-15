//! SAE J2534 pass-thru backend.
//!
//! `consts` holds the spec's values. Version-specific bindings and sessions live
//! under `v0500` (and, later, `v0404`); nothing outside this module depends on
//! which API version an adapter speaks.

pub mod consts;
#[cfg(feature = "j2534")]
pub mod discovery;
#[cfg(feature = "j2534")]
pub mod v0500;

// J2534 drivers register 32-bit DLLs; a 64-bit Windows build could load none.
#[cfg(all(feature = "j2534", windows, target_pointer_width = "64"))]
compile_error!("Build for i686-pc-windows-gnu: Windows J2534 drivers are 32-bit DLLs");
