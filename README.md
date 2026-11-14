# Stanley Charger

Stanley Charger reads battery data from a JBD BMS over Bluetooth Low Energy. It sets the
voltage and the current on a Tenma 72-2605 programmable power supply. When the battery
reaches the SoC limit, the tool switches the supply output off and exits.

## Charging phases

| Battery voltage | Phase | Target voltage | Target current |
| --------------- | ----- | -------------- | -------------- |
| Below 13.6 V | BULK | 16.00 V | 3.0 A |
| 13.6 V to 14.2 V | TAPER | 14.40 V | Falls from 3.0 A to 0.1 A |
| 14.2 V and above | FLOAT | 13.60 V | 0.1 A |

The tool reads the BMS every 10 seconds. `--max-current` caps the target current.
`--limit` sets the SoC limit. After three readings at or above the limit, the tool
switches the output off, so the stop takes about 30 seconds.

## Behaviour when a link fails

- The tool reopens the serial port when a write fails, when the supply stays silent for
  30 seconds, or when USB re-enumerates the device.
- The tool waits for the supply to appear on the USB bus. The tool does not exit when the
  supply loses its USB connection or loses power.
- The tool switches the output off when no battery data arrives for 90 seconds. It
  resumes the charge when the BMS reports again.
- At the SoC limit, the tool repeats the OFF command. If the supply never accepts it, the
  tool reopens the port and switches the output off there. Charging does not resume.

## Requirements

- A JBD BMS with the battery service. Put its MAC address in `TARGET_MAC` in `src/main.rs`.
- A Tenma 72-2605 supply. `lsusb` reports it as `0416:5011`. The tool talks to it at
  9600 baud.
- A Linux host with Bluetooth and USB serial access. Venus OS on a Cerbo GX works.
- Root access, or another user with write access to `/dev/ttyACM*`.

## Trust the BMS

Run these commands once for each battery. Start `bluetoothctl`, then run `scan on` and
wait for the battery to appear in the list. The example below shows the output.

```sh
bluetoothctl
[bluetooth]# scan on
[CHG] Controller 10:BB:F3:C5:A0:17 Discovering: yes
[NEW] Device E1:4B:E8:B0:75:CA Fence
[NEW] Device CF:2A:C7:3C:90:1E House Wall
[NEW] Device A5:C2:37:02:49:87 DP04S007L4S100A
[NEW] Device E3:66:56:65:1E:15 Carport North
[NEW] Device C3:D5:DD:E0:0C:08 Carport South
[bluetooth]# trust A5:C2:37:02:49:87
[CHG] Device A5:C2:37:02:49:87 Trusted: yes
Changing A5:C2:37:02:49:87 trust succeeded
[bluetooth]# scan off
[bluetooth]# exit
```

1. Start `bluetoothctl`.
2. Run `scan on`.
3. Find the battery in the list and note its MAC address.
4. Run `trust <MAC>`.
5. Put that MAC address in `TARGET_MAC` in `src/main.rs`.
6. Run `scan off`.
7. Run `exit`.

## Build and deploy

The `Makefile` needs the `armv7-unknown-linux-gnueabihf` Rust target and `patchelf`.

```sh
make deploy
```

`make deploy` builds the release binary, sets the interpreter to
`/lib/ld-linux-armhf.so.3` for Venus OS, and copies the binary to `/root/stanley-charger`
on the Cerbo. Set `REMOTE_HOST` in the `Makefile` to change the host.

To build without deploying, run these commands:

```sh
cargo build --target armv7-unknown-linux-gnueabihf --release
patchelf --set-interpreter /lib/ld-linux-armhf.so.3 target/armv7-unknown-linux-gnueabihf/release/stanley-charger
```

## Run

```sh
./stanley-charger --limit 80 --max-current 2.0
```

| Option | Effect |
| ------ | ------ |
| `-l, --limit <percent>` | Switch the output off after three readings at or above this SoC. Without it, the tool charges until you stop it. |
| `-i, --max-current <amps>` | Cap the target current below the phase current of 3.0 A. Without it, the tool uses the current for the active phase. |

`Ctrl+C` stops the tool, and the tool does not switch the output off first. Switch the
supply output off on the front panel before you stop the tool.

Keep the tool running when you close the shell:

```sh
screen ./stanley-charger --limit 80
screen -ls
screen -r
```

Press `Ctrl+A` then `D` to detach the screen session.

## Limitations

- The BMS MAC address lives in `TARGET_MAC`. A second battery needs a code change and a
  rebuild.
- Nothing restarts the tool after a crash, and the output has no timestamps.
- `src/decode.rs` decodes four cells only. It ignores the rest of packet 2.
- No test covers the charging phases or the decoder.
