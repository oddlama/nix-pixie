//! The target's clock, set from the controller's (`SetClock`): installers
//! often boot without a real-time clock, or with one months off, and TLS,
//! certificates and signed UEFI variables all go by the date.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nix_pixie_proto::ctrl::TargetMsg;
use rustix::time::{ClockId, Timespec};
use tracing::{debug, info, warn};

use crate::screen;

/// How far off a clock may be before it is set: a link's latency, and
/// whatever NTP will smooth out anyway.
pub const TOLERANCE: Duration = Duration::from_secs(2);

/// How far `off_ms` is, for people: `3 s ahead`, `143 days behind`.
#[must_use]
pub fn describe(off_ms: i64) -> String {
    let direction = if off_ms < 0 { "behind" } else { "ahead" };
    let secs = off_ms.unsigned_abs() / 1000;
    let amount = match secs {
        0..120 => format!("{secs} s"),
        120..7200 => format!("{} min", secs / 60),
        7200..172_800 => format!("{} h", secs / 3600),
        _ => format!("{} days", secs / 86_400),
    };
    format!("{amount} {direction}")
}

/// Milliseconds since 1970 now, or 0 for a clock before it.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// On the target: sets the clock to `unix_ms` if it is off by more than
/// [`TOLERANCE`], and the real-time clock with it, so the installed system
/// starts with the right time too.
pub async fn set(unix_ms: u64) -> TargetMsg {
    let off_ms = i64::try_from(i128::from(now_ms()) - i128::from(unix_ms)).unwrap_or(i64::MAX);
    if off_ms.unsigned_abs() <= u64::try_from(TOLERANCE.as_millis()).unwrap_or(u64::MAX) {
        debug!(off_ms, "the clock is right");
        return TargetMsg::Clock { off_ms, set: false };
    }
    let to = Duration::from_millis(unix_ms);
    let timespec = Timespec {
        tv_sec: i64::try_from(to.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(to.subsec_nanos()),
    };
    if let Err(e) = rustix::time::clock_settime(ClockId::Realtime, timespec) {
        warn!(off_ms, "setting the clock: {e}");
        return TargetMsg::ClockFailed {
            error: format!("setting the clock: {e}"),
        };
    }
    let off = describe(off_ms);
    info!(%off, "set the clock from the controller's");
    screen::say(format!(
        "The clock was {off}; set it from the controller's."
    ));
    if Path::new("/dev/rtc0").exists() {
        set_rtc().await;
    }
    TargetMsg::Clock { off_ms, set: true }
}

/// The real-time clock from the system's, in UTC as NixOS keeps it.
async fn set_rtc() {
    let result = tokio::process::Command::new("hwclock")
        .args(["--systohc", "--utc"])
        .kill_on_drop(true)
        .output()
        .await;
    match result {
        Ok(out) if out.status.success() => debug!("set the real-time clock"),
        Ok(out) => warn!(
            "setting the real-time clock: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => warn!("setting the real-time clock: running hwclock: {e}"),
    }
}
