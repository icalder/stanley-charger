# Stanley Charger

Stanley Charger reads battery data from a JBD BMS over Bluetooth Low Energy. It sets the
voltage and the current on a Tenma 72-2940 programmable power supply, and checks that each
command took effect. When the battery reaches the SoC limit, the tool switches the supply
output off and exits.

## Charging phases

| Battery voltage | Phase | Target voltage | Target current |
| --------------- | ----- | -------------- | -------------- |
| Below 13.6 V | BULK | 16.00 V | 3.0 A |
| 13.6 V to 14.2 V | TAPER | 14.40 V | Falls from 3.0 A to 0.1 A |
| 14.2 V and above | FLOAT | 13.60 V | 0.1 A |

The tool reads the BMS every 10 seconds. `--max-current` caps the target current.
`--limit` sets the SoC limit. After three readings at or above the limit, the tool
switches the output off, so the stop takes about 30 seconds.

## Starting up

The tool opens the supply and forces the output off before anything else. It keeps the
output off until the first BMS reading arrives, then programs the phase targets and
switches on. If no charge row ever appears, look at Bluetooth: the supply has not been
switched on yet.

The supply answers the commands it accepts and stays silent for the commands it acts on.
`VSET1:`, `ISET1:`, `OUT1`, and `OUT0` produce no reply, so the tool confirms them by
reading `VSET1?`, `ISET1?`, and `STATUS?`. A write that no read confirms is reported as a
failure, not as a success.

### Venus OS takes the serial port

On Venus OS, `serial-starter` starts a driver for every serial port it does not recognise.
Those drivers put the port into canonical mode at a different baud rate. The supply then
answers nothing while its output still carries current, which looks like a dead supply.
Tell `serial-starter` to leave the port alone. This is one file, and it is the only change
the tool needs on the Cerbo besides the binary. Run this once:

```sh
mkdir -p /data/conf/serial-starter.d
printf 'alias default\tignore\n' > /data/conf/serial-starter.d/10-tenma-psu.conf
svc -t /service/serial-starter
```

Check it took:

```sh
grep "10-tenma-psu" /var/log/serial-starter/current   # serial-starter read the file
ps | grep -E "gps_dbus|vedirect-dbus"                  # no line mentions ttyACM0
ls /service/*ttyACM0*                                  # nothing matches, after a reboot
```

`/var/log/serial-starter/current` keeps history, so the `Start service gps-dbus.ttyACM0`
lines written before the change stay in the log. Judge by the `ps` line. The
`/service/*ttyACM0*` entries `serial-starter` created earlier are flagged `down`, run
nothing, and are rebuilt from config at boot, so the `ls` line only proves anything after a
reboot.

| Question | Answer |
| -------- | ------ |
| What does it cover? | Every port that falls through to the stock `default` rule, not only `ttyACM0`. See [What else the file changes](#what-else-the-file-changes). |
| How do I undo it? | Delete the file, then run `svc -t /service/serial-starter`. |
| Does it survive a reboot or a firmware update? | Yes, checked on 2026-11-21: after a firmware update the file was still in place, `serial-starter` read it, and no `/service/*ttyACM0*` entry came back. |

### Why the file is not narrower

`serial-starter` reads three keywords: `service`, `alias`, and `include`. Its config cannot
name a port, so the only change open to you without writing to the read-only rootfs is to
redefine the `default` alias. A per-port change needs a udev rule, and the udev rules live on
that rootfs.

### What else the file changes

`serial-starter` starts `default` only for a device that Venus OS does not already name. The
names come from `/etc/udev/rules.d/serial-starter.rules`, so a named device keeps its driver
and never consults `default`. Checked on a Cerbo GX running this file:

| Device | Service | Effect of the file |
| ------ | ------- | ------------------ |
| Built-in port `ttyS4` | `mk2-dbus.ttyS4` | none |
| Built-in ports `ttyS5`, `ttyS6`, `ttyS7` | `vedirect-interface.ttyS5` and so on | none |
| VE.Direct cable (`ID_MODEL=VE_Direct_cable`) | `vedirect-interface.ttyUSB0` | none |
| CAN port | `vecan-dbus.can0` | none |
| Relay outputs `/dev/gpio/relay_1` and `/dev/gpio/relay_2`, digital inputs | GPIO over dbus, never a serial port | none |
| USB-RS485 cable and ISO stick (`rs485` alias) | `cgwacs`, `fzsonick`, `imt`, `modbus` | none |

Five of the nine rules in that udev file end in `:default`, so those devices lose the
`default` part too.
The named driver still starts. Only the extra gps and VE.Direct probing stops.

| `ID_MODEL` or USB id | stock list | with the file |
| -------------------- | ---------- | ------------- |
| `FT232R_USB_UART` | `rs485:default` | `rs485` |
| `FT232EX` | `vedirect:default` | `vedirect` |
| `USB-Serial_Controller_D` | `cgwacs:default` | `cgwacs` |
| `CP2102_USB_to_UART_Bridge_Controller` | `cgwacs:default` | `cgwacs` |
| `1a86:7523`, a CH340 | `cgwacs:default` | `cgwacs` |

One device type to remember: a GPS dongle on an FTDI chip. `default` is what handed it to
`gps-dbus`, so with this file in place Venus OS no longer detects it as a GPS. Move the file
aside while you set up such a device, then put it back.

Two smaller points:

- For a device whose list ends in `:default`, the expansion becomes for example
  `rs485:ignore`, and `serial-starter` logs `ERROR: unknown service ignore`. The named driver
  starts, so this is log noise.
- `serial-starter` reads `/data/conf/serial-starter.d/*.conf` in name order. This file is
  `10-tenma-psu.conf`, so a file from another package named `20-…conf` that also redefines
  `alias default` replaces it. Check `grep -l alias /data/conf/serial-starter.d/*` if a serial
  device stops being detected.

Any root process can change the port at any time, so the tool sets raw mode at 9600 baud
before every exchange and reports it when the settings had changed:

```text
the serial port had been reconfigured (canonical mode holds back the supply's unterminated
replies); raw 9600 baud was restored
```

## What one report looks like

One query gets one answer, 41 bytes, which Bluetooth hands over in pieces of 20 bytes:

```text
dd 03 00 22 05 2d 00 d0 0a ab 29 04 00 9f 2e aa 00 00 00 00
00 00 25 1b 03 04 01 0a dd 00 00 00 28 32 0a ab 00 00 fa 45
77
```

`dd`, command `0x03`, a 16-bit length of `0x22` = 34, 34 bytes of data, one byte that is
`fa` in every frame captured so far, one sum check, then `77`. The check is the two's
complement of the sum of the length bytes and the data bytes, so a whole frame adds up to
zero. The tool buffers the pieces and takes frames by that declared length, because
nothing in a piece says where an answer began.

| Data offset | Field | In the frame above |
| ----------- | ----- | ------------------ |
| 0 | pack voltage, centivolts | 13.25 V |
| 2 | current, centiamps, positive into the battery | +2.08 A |
| 4 | amp-hours left, centiamp-hours | 27.31 Ah |
| 6 | rated capacity, centiamp-hours | 105.00 Ah |
| 8 | cycles | 159 |
| 19 | state of charge, percent | 27% |
| 20 | mosfet state | 3 |
| 21 | cells | 4 |
| 22 | temperature probes | 1 |
| 23 | first probe, tenths of kelvin | 5.0 C |

Offsets 10, 16, and 22 through 33 hold numbers nobody has named yet, and offset 30 repeats
the amp-hours left. The state of charge and the current were checked against the vendor's
phone app, which is what makes these offsets evidence rather than a guess. Cell voltages
are not read: that answer comes from command `0x04`, which this tool does not ask for.

A report is forwarded to the charge loop only when a whole frame has been read. A battery
that goes quiet therefore looks like silence, and the 90 second failsafe below means what
it says.

## Behaviour when a link fails

- The tool reopens the serial port when a write fails, when the supply stays silent for
  30 seconds, or when USB re-enumerates the device.
- The tool waits for the supply to appear on the USB bus. The tool does not exit when the
  supply loses its USB connection or loses power.
- The tool switches the output off when no battery data arrives for 90 seconds. It
  resumes the charge when the BMS reports again.
- The tool switches the output off when it stops for any reason: the SoC limit, `Ctrl+C`,
  `SIGTERM`, or a fault. It exits with status 1 when the supply never confirms the output
  is off, and says so.
- The tool reopens the serial port when the supply answers nothing for 30 seconds, because
  a supply that cannot be talked to cannot be controlled either.
- The tool stops charging when the supply answers but will not accept its commands for 60
  seconds. The output is switched off first. Charging does not resume.
- The tool answers the battery every 10 seconds and forwards what it reads. Fragments that
  complete nothing are normal, and are not reported.

## Requirements

- A JBD BMS with the battery service. Put its MAC address in `TARGET_MAC` in `src/main.rs`.
- A Tenma 72-2940 supply, or another Korad/RAVNI-protocol supply. `lsusb` reports it as
  `0416:5011`. The tool talks to it at 9600 baud.
- A Linux host with Bluetooth and USB serial access. Venus OS on a Cerbo GX works.
- Root access, or another user with write access to `/dev/ttyACM*`.
- On Venus OS, the one-time `serial-starter` setting under
  [Venus OS takes the serial port](#venus-os-takes-the-serial-port). Without it another
  process takes the port and the supply looks dead.

## One battery, one radio at a time

The BMS serves one central, and it stops advertising while connected. Your phone app and
this tool cannot both hold it. When the app fails to connect, find out who owns the link:

```sh
ps | grep -i [s]tanley    # the tool is running, so it owns the battery
hcitool con               # a leftover LE link, even with no process running
```

Stopped with `Ctrl+C` or `SIGTERM`, the tool disconnects and prints:

```text
Battery link released. A phone app can connect now.
```

A link can still be left behind when the tool is killed with `SIGKILL` or crashes, or when
an interrupt arrives while a connect is still in flight, because BlueZ keeps a link up
after the process that opened it disappears. That is why the battery is trusted: see
[Trust the BMS](#trust-the-bms). The tool asks BlueZ for the link back in that case and
prints `Battery link dropped. A phone app can connect now.` When you have to ask yourself:

```sh
bluetoothctl disconnect A5:C2:37:02:49:87
```

Use the adapter BlueZ names in its own answer. On this Cerbo the battery sits on `hci1`, so
the kernel-level fallback needs the same number, and asking the wrong one does nothing
useful and then waits:

```sh
btmgmt -i hci1 disconnect A5:C2:37:02:49:87
```

If the tool cannot connect, it says so within 30 seconds rather than waiting on BlueZ.
The usual reason is that the battery is already held by something else, most often the
phone app: it serves one central and stops advertising while connected.

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

Do not copy a binary that has not been through `patchelf`. A build on Nix names an
interpreter inside the Nix store, which the Cerbo does not have, and the copy answers
`cannot execute: required file not found` however it was copied. `make patch` does the build
and the interpreter in one step.

A running binary cannot be overwritten; the copy fails with `Failure` while the tool is
still up. Stop it first.

Nothing keeps a way back. `make deploy` overwrites the binary on the Cerbo, and a binary
left there for older testing was once cleared by a firmware update. When you want the option
of going back, copy the current one aside first, and remember that git holds every build
this repo has committed:

```sh
ssh root@einstein 'cp /data/home/root/stanley-charger /data/home/root/stanley-charger.prev'
```

## Run

```sh
./stanley-charger --limit 80 --max-current 2.0
```

| Option | Effect |
| ------ | ------ |
| `-l, --limit <percent>` | Switch the output off after three readings at or above this SoC. Without it, the tool charges until you stop it. |
| `-i, --max-current <amps>` | Cap the target current below the phase current of 3.0 A. Without it, the tool uses the current for the active phase. |
| `--watch-battery` | Read the battery and print every piece it sends. The serial port is never opened, so the supply keeps whatever it was last told. |

`Ctrl+C` and `SIGTERM` switch the supply output off, wait for the supply to confirm it, and
then exit. When the supply never confirms, the tool prints a warning and exits with status
1. Check the front panel in that case.

Keep the tool running when you close the shell:

```sh
screen ./stanley-charger --limit 80
screen -ls
screen -r
```

Press `Ctrl+A` then `D` to detach the screen session.

## Reading the battery without touching the supply

To see what the battery sends while the supply stays exactly where you left it:

```sh
./stanley-charger --watch-battery
```

This opens Bluetooth only. The serial port is never opened, so no setpoint changes and the
output is not switched. Every piece that arrives is printed with its length, what it was,
and its bytes, and `Ctrl+C` or `SIGTERM` prints a tally: how many reports were read, how
many frames did not add up, and how many bytes were dropped looking for a frame start.

The tally matters because a decoder that reads nothing looks exactly like a battery that
sends nothing. A run where nothing was read ends with:

```text
NOT ONE COMPLETE REPORT: the battery answered, nothing was read
```

## Limitations

- The BMS MAC address lives in `TARGET_MAC`. A second battery needs a code change and a
  rebuild.
- Nothing restarts the tool after a crash, and the output has no timestamps.
- Cell voltages are not read. The tool would have to ask command `0x04`, and nothing asks
  it yet. Three fields in the status answer are still unnamed.
- The tests cover the charging phases, the BMS decoder, the supply reply parsing, and the
  charge loop against a simulated supply. Nothing tests the USB link itself, so that part
  is checked on the bench.
