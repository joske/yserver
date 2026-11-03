//! Window-system input to host input events, OS-neutral so Linux CI tests
//! it: keycodes, modifier and lock state, buttons, pointer position and
//! scrolling. The shell extracts plain values; these modules decide.

pub(crate) mod devices;
pub(crate) mod keymap;
pub(crate) mod pointer;
#[cfg(any(windows, test))]
pub(crate) mod scancode;
pub(crate) mod translate;
