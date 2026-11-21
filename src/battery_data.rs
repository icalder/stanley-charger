/// One battery report from the BMS, as needed to decide what the supply should do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatteryData {
    /// Terminal voltage at the pack, volts.
    pub voltage: f32,
    /// Current through the pack's own shunt, amperes. Positive is charging.
    pub current: f32,
    /// State of charge as the BMS reports it, percent.
    pub soc: u8,
}

#[cfg(test)]
mod tests {
    use super::BatteryData;

    #[test]
    fn a_report_copies_by_value() {
        let report = BatteryData {
            voltage: 13.17,
            current: -2.5,
            soc: 24,
        };
        let copy = report;

        assert_eq!(copy.voltage, 13.17);
        assert_eq!(copy.current, -2.5, "discharging keeps its sign");
        assert_eq!(copy.soc, 24);
    }
}
