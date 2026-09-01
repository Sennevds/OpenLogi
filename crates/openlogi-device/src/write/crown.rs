//! HID++ `0x4600 Crown` state reads, for diagnostics.
//!
//! The agent does not write the crown through this path — its divert lives in
//! the capture session that also consumes the events (see
//! `crate::session::crown`), because a divert without a listener would only
//! take the dial's native function away. This module is the read-only view:
//! what the crown reports about itself, and who currently owns it.

use std::sync::Arc;

use hidpp::channel::HidppChannel;
use hidpp::device::Device;
use hidpp::feature::CreatableFeature;
use hidpp::feature::crown::{
    CrownControlCapabilities, CrownFeature, CrownSensorCapabilities, RatchetMode, ReportingMode,
};

use super::{HidppOperation, WriteError, classify_hidpp_error, open_feature, with_route};
use crate::backend::HidBackend;
use crate::channel::route::DeviceRoute;

/// What a crown reports about itself and its current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a diagnostic DTO of the crown's independent capability bits; naming each keeps `diag crown` output explicit"
)]
pub struct CrownState {
    /// Slots (fine steps) per full revolution.
    pub slots_per_revolution: u16,
    /// Ratchet detents per full revolution.
    pub ratchets_per_revolution: u16,
    /// Whether the crown has a physical button.
    pub has_button: bool,
    /// Whether the crown detects a touch tap.
    pub has_tap: bool,
    /// Whether the crown reports a touch phase (finger on/off), which is what
    /// a host-side short-tap rule would time against.
    pub has_touch: bool,
    /// Whether the crown reports a proximity phase (hand nearby).
    pub has_proximity: bool,
    /// Whether the crown detects a double tap distinct from a single one.
    pub has_double_tap: bool,
    /// Whether the button distinguishes a long press from a short one.
    pub has_long_press: bool,
    /// Whether the firmware's short/long press boundary is configurable.
    pub short_long_configurable: bool,
    /// Whether the firmware's double-tap window is configurable.
    pub double_tap_configurable: bool,
    /// Current rotation timeout, in 10 ms steps.
    pub rotation_timeout: u8,
    /// Current short/long press boundary, in 10 ms steps.
    pub short_long_timeout: u8,
    /// Current double-tap window, in 10 ms steps.
    pub double_tap_speed: u8,
    /// Whether rotation currently goes to HID++ instead of the firmware's
    /// native function. `None` for a mode byte outside the spec's values.
    pub diverted: Option<bool>,
    /// Whether the crown is in ratchet (detented) rather than free-spin mode.
    /// `None` for a mode byte outside the spec's values.
    pub ratcheting: Option<bool>,
}

/// Read the crown state of the device `route` reaches.
pub async fn get_crown_state(
    backend: &dyn HidBackend,
    route: &DeviceRoute,
) -> Result<CrownState, WriteError> {
    with_route(backend, route, |chan| async move {
        let mut device = open_device(&chan, route.device_index()).await?;
        let crown = open_feature::<CrownFeature>(&mut device).await?;
        let read_failed = |e| classify_hidpp_error(e, HidppOperation::ReadCrown, CrownFeature::ID);
        let info = crown.get_info().await.map_err(read_failed)?;
        let mode = crown.get_mode().await.map_err(read_failed)?;
        Ok(CrownState {
            slots_per_revolution: info.slots,
            ratchets_per_revolution: info.ratchets,
            has_button: info.controls.contains(CrownControlCapabilities::BUTTON),
            has_tap: info.sensors.contains(CrownSensorCapabilities::TAP_GESTURE),
            has_touch: info.sensors.contains(CrownSensorCapabilities::TOUCH),
            has_proximity: info.sensors.contains(CrownSensorCapabilities::PROXIMITY),
            has_double_tap: info
                .sensors
                .contains(CrownSensorCapabilities::DOUBLE_TAP_GESTURE),
            has_long_press: info
                .controls
                .contains(CrownControlCapabilities::BUTTON_LONG_PRESS),
            short_long_configurable: info
                .controls
                .contains(CrownControlCapabilities::SHORT_LONG_TIMEOUT_CONFIGURABLE),
            double_tap_configurable: info
                .controls
                .contains(CrownControlCapabilities::DOUBLE_TAP_SPEED_CONFIGURABLE),
            rotation_timeout: mode.rotation_timeout,
            short_long_timeout: mode.short_long_timeout,
            double_tap_speed: mode.double_tap_speed,
            // `NoChange` is a write-only sentinel; a device answering a *read*
            // with it is reporting something the spec does not define, so it
            // is surfaced as unknown rather than guessed either way.
            diverted: match mode.diverting {
                ReportingMode::Diverted => Some(true),
                ReportingMode::Hid => Some(false),
                _ => None,
            },
            ratcheting: match mode.ratchet_mode {
                RatchetMode::Ratchet => Some(true),
                RatchetMode::Free => Some(false),
                _ => None,
            },
        })
    })
    .await
}

async fn open_device(channel: &Arc<HidppChannel>, index: u8) -> Result<Device, WriteError> {
    Device::new(Arc::clone(channel), index)
        .await
        .map_err(|_| WriteError::DeviceUnreachable { index })
}
