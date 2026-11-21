use clap::Parser;

/// Command line options.
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Maximum battery charge percentage (SoC) limit
    #[arg(short, long)]
    pub limit: Option<u8>,

    /// Maximum charge current (Amperes)
    #[arg(short = 'i', long)]
    pub max_current: Option<f32>,

    /// Watch the battery only, and print every notification with what the decoder makes
    /// of it. The serial port is not opened, so the supply keeps the state you left it in.
    #[arg(long)]
    pub watch_battery: bool,
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;

    #[test]
    fn both_options_can_be_given() {
        let args = Args::try_parse_from(["charger", "--limit", "80", "--max-current", "1.5"])
            .expect("both options are valid");

        assert_eq!(args.limit, Some(80));
        assert_eq!(args.max_current, Some(1.5));
    }

    #[test]
    fn both_options_are_optional() {
        // Charging with no limit is allowed, so a bare invocation must parse.
        let args = Args::try_parse_from(["charger"]).expect("no options are valid");

        assert_eq!(args.limit, None);
        assert_eq!(args.max_current, None);
    }

    #[test]
    fn the_short_current_option_is_not_the_limit() {
        // -l is the SoC limit and -i the current, and mixing them up would set the
        // wrong one on the supply.
        let args =
            Args::try_parse_from(["charger", "-l", "90", "-i", "2"]).expect("short flags parse");

        assert_eq!(args.limit, Some(90));
        assert_eq!(args.max_current, Some(2.0));
    }

    #[test]
    fn a_current_that_is_not_a_number_is_refused() {
        assert!(Args::try_parse_from(["charger", "--max-current", "lots"]).is_err());
    }

    #[test]
    fn watching_the_battery_is_off_unless_asked() {
        // Charging is the normal job, and a run that opens no serial port must be
        // something nobody turns on by accident.
        assert!(
            !Args::try_parse_from(["charger"])
                .expect("no options are valid")
                .watch_battery
        );
        assert!(
            Args::try_parse_from(["charger", "--watch-battery"])
                .expect("the option is valid")
                .watch_battery
        );
    }
}
