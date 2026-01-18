/// Represents the target charging parameters for a given battery voltage.
#[derive(Debug, Clone, Copy)]
pub struct ChargingParameters {
    pub target_voltage: f32,
    pub target_current: f32,
    pub phase: &'static str,
}

/// Determine the charging phase name based on target voltage.
fn get_phase_name(target_voltage: f32) -> &'static str {
    if (target_voltage - 16.0).abs() < 0.01 {
        "BULK"
    } else if (target_voltage - 13.6).abs() < 0.01 {
        "FLOAT"
    } else {
        "TAPER"
    }
}

/// Calculate target voltage and current based on battery voltage.
/// Implements a three-phase charging algorithm:
/// - BULK (< 13.6V): High push at 16.00V, 5.0A
/// - TAPER (13.6V - 14.2V): Ramp down current from 5.0A to 0.1A at 14.40V
/// - FLOAT (≥ 14.2V): Maintenance charge at 13.60V, 0.1A
pub fn calculate_charging_parameters(battery_voltage: f32) -> ChargingParameters {
    let target_voltage = if battery_voltage < 13.6 {
        // BULK: High push to overcome wire resistance
        16.00
    } else if battery_voltage >= 14.2 {
        // FLOAT: Battery is full, drop to a resting voltage
        13.60
    } else {
        // ABSORPTION/TAPER: Close to full, move to standard charging voltage
        14.40
    };

    let target_current = if battery_voltage < 13.6 {
        5.0
    } else if battery_voltage >= 14.2 {
        0.1
    } else {
        (5.0 - ((battery_voltage - 13.6) / 0.6) * 4.5).clamp(0.1, 5.0)
    };

    let phase = get_phase_name(target_voltage);

    ChargingParameters {
        target_voltage,
        target_current,
        phase,
    }
}
