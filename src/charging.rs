/// Voltage the supply applies while the battery sits below `TAPER_START_VOLTAGE`.
/// High to overcome wire resistance.
const BULK_VOLTAGE: f32 = 16.00;
/// Voltage the supply applies inside the TAPER window (absorption).
const TAPER_VOLTAGE: f32 = 14.40;
/// Maintenance voltage the supply applies once the battery is full.
const FLOAT_VOLTAGE: f32 = 13.60;

/// Battery voltage where BULK ends and TAPER starts.
const TAPER_START_VOLTAGE: f32 = 13.60;
/// Battery voltage where TAPER ends and FLOAT starts.
const FLOAT_START_VOLTAGE: f32 = 14.20;

/// Highest charge current, used in BULK and at the bottom of the TAPER ramp.
const MAX_CHARGE_CURRENT: f32 = 3.0;
/// Maintenance current in FLOAT, and where the TAPER ramp lands at `FLOAT_START_VOLTAGE`.
const FLOAT_CURRENT: f32 = 0.1;

/// Battery voltage span covered by TAPER.
const TAPER_WINDOW: f32 = FLOAT_START_VOLTAGE - TAPER_START_VOLTAGE;
/// Current removed per volt inside the TAPER window, so the ramp lands on FLOAT_CURRENT.
const TAPER_SLOPE: f32 = (MAX_CHARGE_CURRENT - FLOAT_CURRENT) / TAPER_WINDOW;

/// Represents the target charging parameters for a given battery voltage.
#[derive(Debug, Clone, Copy)]
pub struct ChargingParameters {
    pub target_voltage: f32,
    pub target_current: f32,
    pub phase: &'static str,
}

/// Calculate target voltage and current based on battery voltage.
/// Implements a three-phase charging algorithm:
/// - BULK (< 13.6V): High push at 16.00V, 3.0A
/// - TAPER (13.6V - 14.2V): Hold 14.40V, ramp current from 3.0A down to 0.1A
/// - FLOAT (>= 14.2V): Maintenance charge at 13.60V, 0.1A
pub fn calculate_charging_parameters(battery_voltage: f32) -> ChargingParameters {
    let (target_voltage, target_current, phase) = if battery_voltage < TAPER_START_VOLTAGE {
        (BULK_VOLTAGE, MAX_CHARGE_CURRENT, "BULK")
    } else if battery_voltage >= FLOAT_START_VOLTAGE {
        (FLOAT_VOLTAGE, FLOAT_CURRENT, "FLOAT")
    } else {
        let ramp = MAX_CHARGE_CURRENT - (battery_voltage - TAPER_START_VOLTAGE) * TAPER_SLOPE;
        let current = ramp.clamp(FLOAT_CURRENT, MAX_CHARGE_CURRENT);
        (TAPER_VOLTAGE, current, "TAPER")
    };

    ChargingParameters {
        target_voltage,
        target_current,
        phase,
    }
}
