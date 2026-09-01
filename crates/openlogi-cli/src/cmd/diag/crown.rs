//! `openlogi diag crown` — inspect the Craft keyboard's crown dial.

use anyhow::{Context, Result};
use clap::Args;
use openlogi_hid::CrownState;

use crate::cmd::diag::select_device;

#[derive(Debug, Args)]
pub struct CrownArgs {
    /// Run against the device whose name contains this string
    /// (case-insensitive) instead of auto-selecting.
    #[arg(long, value_name = "NAME")]
    pub device: Option<String>,
}

pub async fn run(args: CrownArgs) -> Result<()> {
    let (route, name) = select_device(args.device.as_deref(), &[0x4600]).await?;
    println!("device: {name} ({route})");

    let state = openlogi_hid::get_crown_state(&route)
        .await
        .context("read crown info and mode")?;
    print_state(state);
    Ok(())
}

fn print_state(state: CrownState) {
    println!(
        "  geometry: {} slots / {} ratchets per revolution",
        state.slots_per_revolution, state.ratchets_per_revolution
    );
    println!(
        "  controls: button={} long-press={} tap={} double-tap={} touch={} proximity={}",
        yes_no(state.has_button),
        yes_no(state.has_long_press),
        yes_no(state.has_tap),
        yes_no(state.has_double_tap),
        yes_no(state.has_touch),
        yes_no(state.has_proximity)
    );
    println!(
        "  timings: rotation={}ms short/long={}ms double-tap={}ms (configurable: short/long={} double-tap={})",
        u32::from(state.rotation_timeout) * 10,
        u32::from(state.short_long_timeout) * 10,
        u32::from(state.double_tap_speed) * 10,
        yes_no(state.short_long_configurable),
        yes_no(state.double_tap_configurable)
    );
    // Who owns the dial right now. `diverted` means some host software (this
    // agent, or Logi Options+) is translating rotation itself, so the crown's
    // native volume function is not running.
    println!(
        "  mode: reporting={} ratchet={}",
        tristate(state.diverted, "diverted (host-owned)", "native (firmware)"),
        tristate(state.ratcheting, "ratchet", "free-spin")
    );
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// A mode byte the spec does not define for a read is reported as unknown
/// rather than collapsed into either state.
fn tristate(
    value: Option<bool>,
    when_true: &'static str,
    when_false: &'static str,
) -> &'static str {
    match value {
        Some(true) => when_true,
        Some(false) => when_false,
        None => "unknown (undefined mode byte)",
    }
}
