use binary_layout::binary_layout;

#[derive(Debug, Default)]
pub enum BatteryDataState {
    #[default]
    Waiting,
    Packet1Received,
    Packet2Received,
    CellDataReceived,
}

impl BatteryDataState {
    fn is_valid_data(&self, data: &Vec<u8>) -> bool {
        match self {
            BatteryDataState::Waiting => data.len() > 2 && data[0] == 0xdd && data[1] == 0x03,
            BatteryDataState::Packet1Received => data.len() > 2 && data[0] != 0xdd,
            BatteryDataState::Packet2Received => {
                data.len() > 2 && data[0] == 0xdd && data[1] == 0x04
            }
            // allow new data to start another cycle
            BatteryDataState::CellDataReceived => {
                data.len() > 2 && data[0] == 0xdd && data[1] == 0x03
            }
        }
    }
}

// See here for more data which code also be decoded:
// https://github.com/neilsheps/overkill-xiaoxiang-jbd-bms-ble-reader/blob/main/src/main.cpp

binary_layout!(battery_data_1, BigEndian, {
  unused: [u8; 3],
  // only set for the first packet
  total_data_length: u8,
  voltage: u16,
  current: i16,
  remaining_capacity: u16,
  nominal_capacity: u16,
  cycles: u16
});

binary_layout!(battery_data_2, BigEndian, {
  unused: [u8; 3],
  soc: u8,
  mosfet_state: u8,
  number_of_cells: u8,
  number_of_temp_sensors: u8,
  temp1: i16
  // TODO other fields
});

binary_layout!(cell_data, BigEndian, {
  unused: [u8; 3],
  number_of_cells: u8,
  // TODO replace this with a for loop for dynamic number of cells
  cell1: u16,
  cell2: u16,
  cell3: u16,
  cell4: u16
});

#[derive(Debug, serde::Serialize)]
pub struct Decoder {
    #[serde(skip)]
    state: BatteryDataState,
    #[serde(skip)]
    total_data_length: u8,
    pub voltage: f32,
    // -ve when discharging
    pub current: f32,
    // -ve when discharging
    pub power: f32,
    #[serde(rename = "charge")]
    pub remaining_capacity: f32,
    pub cycles: u16,
    pub soc: u8,
    pub temperature: f32,
    #[serde(rename = "cellVoltages")]
    pub cell_volts: [u16; 4],
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            state: Default::default(),
            total_data_length: Default::default(),
            voltage: Default::default(),
            current: Default::default(),
            power: Default::default(),
            remaining_capacity: Default::default(),
            cycles: Default::default(),
            soc: Default::default(),
            temperature: Default::default(),
            cell_volts: Default::default(),
        }
    }
}

impl Decoder {
    pub fn reset(&mut self) {
        *self = Default::default();
    }

    pub fn is_soc_ready(&self) -> bool {
        // SoC is populated once we reach or pass Packet2Received
        match self.state {
            BatteryDataState::Packet2Received | BatteryDataState::CellDataReceived => true,
            _ => false,
        }
    }

    /// Returns true when all expected packets have been decoded and the public data fields are valid
    ///
    /// # Errors
    ///
    /// This function may return an error if an invalid data packet is received.
    pub fn decode(&mut self, data: &Vec<u8>) -> Result<bool, String> {
        if !self.state.is_valid_data(data) {
            return Ok(false);
            // return Err(format!(
            //     "invalid data received in state {:?}: {:?}",
            //     self.state, data
            // ));
        }
        match self.state {
            BatteryDataState::Waiting => self.decode_battery_data_pkt1(data),
            BatteryDataState::Packet1Received => self.decode_battery_data_pkt2(data),
            BatteryDataState::Packet2Received => self.decode_cell_data(data),
            BatteryDataState::CellDataReceived => {
                // Reset and start again
                self.reset();
                return self.decode(data);
            }
        }
        Ok(matches!(self.state, BatteryDataState::CellDataReceived))
    }

    fn decode_battery_data_pkt1(&mut self, d: &Vec<u8>) {
        let view = battery_data_1::View::new(d);
        self.total_data_length = view.total_data_length().read();
        self.voltage = view.voltage().read() as f32 / 100.0; // V
        self.current = view.current().read() as f32 / 100.0; // A
        self.power = self.voltage * self.current;
        self.remaining_capacity = view.remaining_capacity().read() as f32 / 100.0; // Ah
        self.cycles = view.cycles().read();
        // TODO other fields
        self.state = BatteryDataState::Packet1Received;
    }

    fn decode_battery_data_pkt2(&mut self, d: &Vec<u8>) {
        let view = battery_data_2::View::new(d);
        self.soc = view.soc().read();
        self.temperature = (view.temp1().read() - 2731) as f32 / 10.0;
        self.state = BatteryDataState::Packet2Received;
    }

    fn decode_cell_data(&mut self, d: &Vec<u8>) {
        let view = cell_data::View::new(d);
        self.cell_volts[0] = view.cell1().read(); // mV
        self.cell_volts[1] = view.cell2().read(); // mV
        self.cell_volts[2] = view.cell3().read(); // mV
        self.cell_volts[3] = view.cell4().read(); // mV
        self.state = BatteryDataState::CellDataReceived;
    }
}
