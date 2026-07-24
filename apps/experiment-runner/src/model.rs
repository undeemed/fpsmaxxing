//! Deterministic measurement model.
//!
//! On the Linux safe-alpha path there is no `PresentMon` or hardware telemetry to
//! sample, so [`measure`] stands in for a live source. It is a pure function of
//! the knob value under test: the same value always yields the same samples,
//! which is what lets a trial be replayed and re-evaluated from the journal
//! alone.
//!
//! The stand-in models three coupled effects of the mock knob (`0..=100`):
//! frames per second and both temperature and power draw rise with the value,
//! and faults appear only once the value climbs past a safe onset. A leading
//! run of `warmup` samples is generated at a cold-start FPS and then discarded,
//! so the counted samples reflect steady state.

use fpsmaxxing_contracts::MetricSample;

/// Frames per second reported while warming up, before steady state.
const COLD_FPS: f64 = 60.0;
/// Steady-state frames per second at the lowest knob value.
const BASE_FPS: f64 = 60.0;
/// Steady-state frames-per-second gain per knob unit.
const FPS_GAIN_PER_UNIT: f64 = 1.0;
/// Component temperature in degrees Celsius at the lowest knob value.
const BASE_TEMPERATURE_C: f64 = 50.0;
/// Additional degrees Celsius per knob unit.
const TEMPERATURE_C_PER_UNIT: f64 = 0.5;
/// Board power draw in watts at the lowest knob value.
const BASE_POWER_W: f64 = 100.0;
/// Additional watts per knob unit.
const POWER_W_PER_UNIT: f64 = 1.0;
/// Knob value at or below which no correctness faults are modeled.
const ERROR_ONSET: u64 = 90;

/// Produces `counted` steady-state samples for a knob value.
///
/// `warmup` leading samples are generated at a cold-start FPS and dropped, so
/// the returned vector always holds exactly `counted` steady-state samples. The
/// output depends only on `value`, `warmup`, and `counted`, never on wall-clock
/// time or external state.
#[must_use]
pub fn measure(value: u64, warmup: u32, counted: u32) -> Vec<MetricSample> {
    let setting = knob_to_setting(value);
    let total = warmup.saturating_add(counted);
    let mut samples: Vec<MetricSample> = (0..total)
        .map(|index| sample_at(setting, value, index < warmup))
        .collect();
    samples.split_off(warmup as usize)
}

/// Builds one deterministic sample at a given setting.
fn sample_at(setting: f64, value: u64, warming: bool) -> MetricSample {
    MetricSample {
        fps: if warming {
            COLD_FPS
        } else {
            BASE_FPS + FPS_GAIN_PER_UNIT * setting
        },
        temperature_c: BASE_TEMPERATURE_C + TEMPERATURE_C_PER_UNIT * setting,
        power_w: BASE_POWER_W + POWER_W_PER_UNIT * setting,
        errors: value.saturating_sub(ERROR_ONSET),
    }
}

/// Converts a bounded knob value into a float setting without a lossy cast.
///
/// Mock values are policy-bounded to `0..=100`, so the `u32` conversion never
/// saturates in practice; the fallback keeps the function total.
fn knob_to_setting(value: u64) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::{BASE_FPS, COLD_FPS, measure};

    #[test]
    fn returns_exactly_the_counted_samples() {
        let samples = measure(40, 3, 5);
        assert_eq!(samples.len(), 5);
    }

    #[test]
    fn warmup_samples_are_discarded() {
        // Every returned sample must be steady state; a cold-start FPS leaking
        // through would prove the warmup prefix was not dropped.
        let samples = measure(40, 4, 6);
        assert!(samples.iter().all(|sample| sample.fps > COLD_FPS));
        assert!(
            samples
                .iter()
                .all(|sample| (sample.fps - (BASE_FPS + 40.0)).abs() < f64::EPSILON)
        );
    }

    #[test]
    fn higher_values_raise_fps_temperature_and_power() {
        let low = &measure(10, 0, 1)[0];
        let high = &measure(60, 0, 1)[0];
        assert!(high.fps > low.fps);
        assert!(high.temperature_c > low.temperature_c);
        assert!(high.power_w > low.power_w);
    }

    #[test]
    fn faults_appear_only_past_the_safe_onset() {
        assert_eq!(measure(90, 0, 1)[0].errors, 0);
        assert_eq!(measure(95, 0, 1)[0].errors, 5);
    }

    #[test]
    fn measurement_is_deterministic() {
        assert_eq!(measure(37, 2, 4), measure(37, 2, 4));
    }
}
