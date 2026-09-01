//! The crown mode indicator, as the rest of the agent sees it.
//!
//! Windows has a real one ([`crate::osd_windows`]); macOS and Linux do not yet.
//! Rather than cfg-gate every call site — which costs a placeholder handle and
//! trips lints that only fire on the platforms without an indicator — this is
//! one type whose non-Windows form does nothing. Same shape as the overlay's
//! `platform` module, whose non-macOS arms are honest no-ops.

#[cfg(not(target_os = "windows"))]
use openlogi_agent_core::CrownModeIndicator;

#[cfg(target_os = "windows")]
pub use crate::osd_windows::Osd;

/// The indicator on a platform that has none.
///
/// Not `Copy`: a by-reference no-op receiver is what keeps the call sites
/// identical across platforms, and `Copy` would make that a lint.
#[cfg(not(target_os = "windows"))]
#[derive(Debug, Clone)]
pub struct Osd;

#[cfg(not(target_os = "windows"))]
impl Osd {
    /// Nothing to start.
    pub fn spawn() -> Self {
        Self
    }

    /// Nothing to draw. The mode change is already in the agent log.
    // `allow`, not `expect`: the lint fires only on the platforms whose `show`
    // is a no-op, so an expectation would be unfulfilled on Windows. Taking
    // `&self` is what keeps the call site identical across platforms.
    #[expect(clippy::allow_attributes, reason = "see above")]
    #[allow(clippy::unused_self, reason = "no-op mirrors the Windows signature")]
    pub fn show(&self, indicator: &CrownModeIndicator) {
        let _ = indicator;
    }
}

/// Start the indicator for this platform.
pub fn spawn() -> Osd {
    #[cfg(target_os = "windows")]
    return crate::osd_windows::spawn();
    #[cfg(not(target_os = "windows"))]
    return Osd::spawn();
}
