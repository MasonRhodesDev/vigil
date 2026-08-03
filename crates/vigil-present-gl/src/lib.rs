//! Reserved crate for the M3 GL presenter (DESIGN.md §2, §7): FemtoVG over
//! GBM/EGL behind the same [`vigil_core::Presenter`] trait, reusing the
//! `OpenGLInterface` approach proven in the slint-headless spike.
//!
//! Deliberately empty until M3 — reserving the crate keeps the seam visible
//! in the workspace and the dependency rules honest. No GL/GBM/EGL
//! dependencies may be added anywhere else in the meantime.
