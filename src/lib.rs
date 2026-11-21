//! Battery charger control for the Stanley rack: BMS protocol decoding, the charge
//! policy, and the supply drivers.
//!
//! `main.rs` is the platform glue — it opens the Bluetooth link to the battery and
//! wires the pieces here together. Keeping the logic in a library means it can be
//! tested without a radio, a supply, or the dbus client the BLE stack needs.

pub mod args;
pub mod battery_data;
pub mod charge_loop;
pub mod charging;
pub mod decode;
pub mod psu;
pub mod tenma;

pub use args::Args;
pub use battery_data::BatteryData;
