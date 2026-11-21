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

#[cfg(test)]
mod tests {
    use super::{
        BULK_VOLTAGE, FLOAT_CURRENT, FLOAT_START_VOLTAGE, FLOAT_VOLTAGE, MAX_CHARGE_CURRENT,
        TAPER_START_VOLTAGE, TAPER_VOLTAGE, calculate_charging_parameters,
    };

    fn targets(volts: f32) -> (f32, f32, &'static str) {
        let params = calculate_charging_parameters(volts);
        (params.target_voltage, params.target_current, params.phase)
    }

    #[test]
    fn a_flat_battery_gets_the_full_bulk_push() {
        assert_eq!(targets(12.0), (BULK_VOLTAGE, MAX_CHARGE_CURRENT, "BULK"));
        assert_eq!(targets(10.5), (BULK_VOLTAGE, MAX_CHARGE_CURRENT, "BULK"));
    }

    #[test]
    fn the_taper_window_holds_absorption_voltage_and_ramps_current_down() {
        let (_, current_at_start, phase_at_start) = targets(TAPER_START_VOLTAGE);
        assert_eq!(phase_at_start, "TAPER");
        assert_eq!(current_at_start, MAX_CHARGE_CURRENT);

        let (volts, current_mid, _) = targets(13.9);
        assert_eq!(volts, TAPER_VOLTAGE);
        assert!(
            (current_mid - 1.55).abs() < 0.01,
            "mid-window should be about half current, got {current_mid}"
        );

        let (volts_at_top, current_at_top, _) = targets(14.19);
        assert_eq!(volts_at_top, TAPER_VOLTAGE);
        assert!(
            current_at_top > FLOAT_CURRENT,
            "the ramp must not undershoot the maintenance current"
        );
    }

    #[test]
    fn a_full_battery_drops_to_maintenance() {
        assert_eq!(
            targets(FLOAT_START_VOLTAGE),
            (FLOAT_VOLTAGE, FLOAT_CURRENT, "FLOAT")
        );
        assert_eq!(targets(14.6), (FLOAT_VOLTAGE, FLOAT_CURRENT, "FLOAT"));
    }

    #[test]
    fn the_phases_meet_at_their_shared_voltage() {
        // A gap between phases would make the supply jump as the battery crosses the
        // boundary, so the current either side of each boundary has to be close.
        let (_, below, _) = targets(TAPER_START_VOLTAGE - 0.01);
        let (_, above, _) = targets(TAPER_START_VOLTAGE);
        assert!(
            (below - above).abs() < 0.2,
            "current jumped {below} -> {above} across the BULK/TAPER boundary"
        );

        let (_, below, _) = targets(FLOAT_START_VOLTAGE - 0.01);
        let (_, above, _) = targets(FLOAT_START_VOLTAGE);
        assert!(
            below < 0.4 && above == FLOAT_CURRENT,
            "current jumped {below} -> {above} across the TAPER/FLOAT boundary"
        );
    }

    #[test]
    fn no_battery_voltage_produces_an_out_of_range_current() {
        // The supply is set from these numbers, so they must stay inside what the
        // pack and the supply are rated for.
        let mut volts = 9.0;
        while volts <= 16.0 {
            let (_, current, phase) = targets(volts);
            assert!(
                (FLOAT_CURRENT..=MAX_CHARGE_CURRENT).contains(&current),
                "at {volts:.2}V the current was {current}"
            );
            assert!(!phase.is_empty(), "at {volts:.2}V the phase was empty");
            volts += 0.05;
        }
    }
}
