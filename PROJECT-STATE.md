# Project State

Inspection date: 2026-11-21, and this round is committed: `Cargo.toml`, `Cargo.lock`,
`src/main.rs`, `src/tenma.rs`, `src/charging.rs`, `src/decode.rs` were modified, and
`src/lib.rs`, `src/args.rs`, `src/battery_data.rs`, `src/psu.rs`, `src/charge_loop.rs` are
new. `README.md` and this file follow in a second commit, and the supply-versus-battery
current check came out in a third. 64 tests pass, ARM clippy is clean over `--all-targets`,
`cargo fmt --check` is clean.
The ARM binary is built, patched, and deployed to `root@einstein:stanley-charger`. The
previous binary is kept on the Cerbo as `stanley-charger.prev`
(md5 `0a43b305763f9809375aad4ac53092c3`, the build before the decoder rewrite).

## What This Round Was About

Charging stopped working on the Cerbo, and the supply became unreadable. The cause was not
the protocol. Venus OS `serial-starter` matched the unknown `/dev/ttyACM0` against its
default rule (`alias default gps:vedirect`) and started `gps-dbus` and
`vedirect-interface` on it. Those programs left the tty in canonical mode at 38400 baud.
A Tenma reply is a bare, unterminated number, so canonical mode holds it in the line
buffer forever: `read()` returns nothing while the supply sits at 14.94 V and 3.00 A. The
supply was healthy the whole time.

Two fixes followed. `/data/conf/serial-starter.d/10-tenma-psu.conf` holds
`alias default	ignore`, so `serial-starter` no longer touches the port. It is the only
file the tool needs on the Cerbo, and `README.md` documents installing, verifying, undoing,
and the blast radius: `serial-starter` in this Venus OS version parses only `service`,
`alias`, and `include`, so a port cannot be named and the alias has to be redefined for the
whole `default` class. The driver now
asserts raw mode at 9600 baud before every exchange and says so when it finds the settings
had changed, because any root process can change them again.

The other three changes closed code gaps in the previous version of this document: verified
writes, supervised output state, and safe shutdown on `Ctrl+C` and `SIGTERM`. A fourth,
comparing the current the supply reports against the current the battery reports, was added
in that round and has since been removed; see the gaps.

## The Report That Never Arrived

Charging then ran on one reading. The BMS answers a query with 41 bytes, Bluetooth hands
them over as 20 + 20 + 1, and the old `decode.rs` matched those pieces against a fixed
order instead of against the length each frame declares: a first packet, then a headerless
continuation, then a frame headed `dd 04`. That last one is the answer to a query this tool
never sends, so the machine decoded one report, parked in `packet2`, and refused every
report after it. A 40-second capture held 33 notifications, 2 accepted, 31 refused, 1
report.

The part that mattered: `run_monitor_loop` forwarded a reading whenever `is_soc_ready()`
was true, and that stays true in `packet2` forever. So the charge loop was handed the same
report it got a few seconds after startup, over and over, each with a fresh timestamp. The
SoC limit was computed from a frozen number and the 90 second stale-data failsafe could
never fire, because new timestamps kept arriving.

`src/decode.rs` is now a reassembler: buffer, take frames by their declared length, check
the sum byte and the `77`, dispatch on the command byte, resynchronise on the next `dd`
after a lost fragment, and count every refusal. `run_monitor_loop` forwards a reading only
when a whole frame was decoded, so silence from the battery looks like silence.

That also settles what looked like a wiring fault. The old gap 1 below said the supply
carried 3 A while the battery reported none. It was the frozen frame, taken before current
was flowing. The phone app agreed with the fresh reading all along.

## Code Map

3793 lines including tests. `src/main.rs` no longer holds types or logic, so the charge
loop is testable without linking the BLE stack.

| File | Lines | Tests | State |
| --- | --- | --- | --- |
| `src/main.rs` | 504 | 0 | Scans, connects, subscribes, and polls the BMS every 10 s. Holds the BMS link in `battery_link` and disconnects it on every exit path. Owns shutdown: `Ctrl+C` or `SIGTERM` closes a `watch` channel and waits `SHUTDOWN_GRACE` (15 s) for the charge task. Exits 1 when the task ends without confirming OFF, so the supervisor's log line is the last thing printed. Connects under `CONNECT_BUDGET` (30 s) inside the same select, so an interrupt works during connect. Holds the battery only (`--watch-battery`), for captures. |
| `src/lib.rs` | 17 | 0 | Library surface. Keeps `Args`, `BatteryData`, and the logic in one crate the tests can link. |
| `src/args.rs` | 54 | 4 | `--limit` and `--max-current`, moved out of `main.rs`. |
| `src/battery_data.rs` | 29 | 1 | The reading the charge loop consumes. |
| `src/psu.rs` | 224 | 8 | `PowerSupply` trait, `OutputStatus`, and `PsuError`. Every operation is confirmed, so a fake can fail one of them on purpose. |
| `src/tenma.rs` | 747 | 12 | Wire driver for one port handle: `TenmaLink`. `connect` waits for `0416:5011`, opens with `custom_args`, captures raw 9600 termios, handshakes with `*IDN?`, and switches the output off. Writes are confirmed by read-back. |
| `src/charge_loop.rs` | 1212 | 18 | Charge policy and supervision: phases, setpoint writes, output state, stale BMS data, silence and trust budgets, shutdown. |
| `src/charging.rs` | 137 | 5 | BULK, TAPER, FLOAT. Pure. |
| `src/decode.rs` | 846 | 15 | Reassembles frames from notifications, checks the end marker and the sum byte, dispatches on the command byte, and resynchronises after a lost fragment. `Frames` keeps the tally that tells a silent battery from a decoder reading nothing. |

## The Contract Below The Loop

`Controller::run` returns one `ControlOutcome`, and each one says what to do with the
port. `ChargeSession` survives a reconnect, and it forgets its cached setpoints whenever
a link closes, because the next handle may be a different supply or a supply an operator
has touched.

- `Finished` / `Stopped`: the run ended and the supply confirmed `STATUS?` says OFF.
- `LinkLost`: the port handle failed. Reopen after 2 s and keep charging.
- `Halted(detail)`: the supply answered but did not accept what it was told for
  `TRUST_BUDGET` (60 s). The output is confirmed off. Charging does not resume.
- `FinishPending(reason)`: the run ended and the output could not be confirmed off. The
  supervisor reports it and exits non-zero. Charging cannot resume, which is the point.

`switch_output_off` is the one place that knows whether a refusal is worth reopening for:
a dead port is `LinkLost`, a supply that says no is `Halted`.

Two budgets, deliberately different. `SILENCE_BUDGET` (30 s) watches whether the supply
answers reads at all; when it stops, the port is reopened. `TRUST_BUDGET` (60 s) watches
whether the supply accepts writes, and it counts from the *first unfulfilled command*, not
from the last successful one. A charge that needs no new commands holds 16.00 V and 3.00 A
for an hour, and must never be mistaken for an untrustworthy supply. A successful read
does not clear the trust clock either, because a supply that answers `VOUT1?` correctly can
still be refusing `VSET1:`.

## Verified And Unverified

Written to the supply, and confirmed by `STATUS?`, before the loop reports success:
`VSET1:`, `ISET1:`, `OUT1`, `OUT0`. Anything the supply answers is range-checked:
`16.00` is a plausible volt reading, `114.94` is not, and `2.999` is a plausible amp
reading while `10.000` is not.

Checked on the bench with the Tenma 72-2940 (V6.0, SN 00105270) on 2026-11-21:

- `*IDN?` over raw 9600 answers `TENMA 72-2940 V6.0 SN:00105270`.
- Startup prints `Tenma found on /dev/ttyACM0; output off.` and
  `Supply answered the handshake: ...`.
- The output stayed off until the first BMS reading, then `>>> [BULK]` appeared with
  `PSU VSET: 16.00V` and `PSU OUT: 14.84V 2.998A`.
- `SIGINT` printed `Interrupted. Switching the supply output off.` then
  `Interrupted. Supply output is off.` and the process exited. `SIGTERM` follows the same
  path in code and has not been sent on the bench.
- After exit, a direct probe read `STATUS?` = `0x12`, `VOUT1?` = `00.00`,
  `IOUT1?` = `0.000`. The supply was genuinely off.
- After the Venus OS firmware update (`/etc/version` 20261022164802), the `serial-starter`
  drop-in survived, `serial-starter` read it, nothing ran on `/dev/ttyACM0`, and
  `/service/*ttyACM0*` did not come back. The deployed binary survived with its md5
  unchanged; `stanley-charger.prev` is gone. Battery trust survived too.
- `Ctrl+C` after the handshake exits `0`, because the supply confirmed OFF. A run that
  ends without that confirmation exits `1`; the choice is `ControlOutcome::output_is_confirmed_off`,
  unit-tested rather than bench-forced, because forcing it needs a supply that will not
  answer `OUT0`.

The battery radio is shared, and a Venus OS BlueZ that trusts a device keeps the link up
after the process that opened it dies. A JBD BMS serves one central, so a leftover link
locks out the phone app. `main.rs` now disconnects on every exit path and prints
`Battery link released. A phone app can connect now.`, and `run_monitor_loop` returns when
the charge task stops or a stop is requested instead of looping until the process dies.

Seen on the bench on 2026-11-21, with the capture binary reading the battery while the
supply was set by hand to 14.4 V and about 2 A:

- One query, one report. A 40-second capture: 15 notifications, 5 reports, 0 corrupt, 0
  bytes dropped resynchronising, five frame starts for five queries, fragments 1 to 20
  bytes.
- The charge loop now sees current moving. `Battery Voltage: 13.26V | Current: +2.77A |
  SoC: 27% | Probe: 5.2C` against `PSU OUT: 14.84V 2.999A`. Two meters on the same wire,
  reading within 0.2 A of each other.
- `SIGTERM` on a watching run stops it: `Stopped: stopped.`, the tally, then
  `Battery link released. A phone app can connect now.` `pkill` sends `SIGTERM`, which the
  watch loop used to ignore.
- `SIGTERM` during connect: `Interrupted. Supply output is off.`, then
  `A connect was still in flight, so a link may be left behind.` and
  `Battery link dropped. A phone app can connect now.` `hcitool con` came back empty, so
  the phone app is not locked out. Without the flag the tool asked BlueZ for a link on
  every clean stop, which cost 5 s and printed a warning nobody needed.
- An interrupt during connect used to leave `LE A5:C2:37:02:49:87 handle 16 lm CENTRAL`
  behind with no process owning it. `bluetoothctl disconnect A5:C2:37:02:49:87` cleared it;
  `btmgmt -i hci0 disconnect` did not, because BlueZ keeps this battery on `hci1` and the
  wrong index makes the command wait for nothing.

A copied binary that has not been through `patchelf` does not run on Venus OS at all: the
interpreter it names lives in the Nix store on the build machine, and the copy answers
`cannot execute: required file not found`. `make patch` exists for this. `scp` over a
running binary also fails, with `Failure` and no explanation.

Seen on the bench on 2026-11-21 earlier in the day, after the firmware update: `hcitool con` listed
`LE A5:C2:37:02:49:87 handle 16 lm CENTRAL` while the tool ran, `Ctrl+C` printed
`Battery link released. A phone app can connect now.`, exited `0` three seconds later, and
`hcitool con` came back empty. The battery was left advertising, so the phone app can
connect. Before that run the BMS had held a dead link, and the reboot is what cleared it.

Not yet seen in the field, though each is unit-tested: the termios repair message, the
silence reopen, and the trust halt. The stale-BMS
failsafe was seen in the previous build: `No battery data for 90 s. Switching the PSU
output off.` followed by `Battery data is back. Resuming charge.`

## Gaps

Ranked by risk to correct charging behaviour.

### 1. Closed: the battery appeared not to see what the supply delivered

Every reading that fed this gap was a frozen frame. The old decoder stopped reporting after
the first report, so the `0.00 A` beside a supply carrying 3 A was the reading taken
seconds after startup, before current flowed, repeated with fresh timestamps. With the
reassembler in place the BMS reports 2.77 A against the supply's 2.999 A, and the phone app
agreed with the BMS throughout. Nothing points at the shunt, the sense leads, or the
wiring.

The comparison this gap grew into has been removed. It was written when the only evidence
available was a decoder reading nothing, and once the frames were read properly the two
meters agreed by themselves. Two meters that measure different things can disagree without
anything being wrong, and the warning had no way to tell a wiring fault from a battery meter
that simply counts a different path. What supervises the charge path now: every write is
confirmed, the output state is checked against what charging wants, the supply's own
readings are printed every cycle, and battery data that stops arriving switches the output
off.

### 2. No logging or service manager

`println!` and `eprintln!` with no timestamps, no rotation, and nothing restarts the tool
after a crash. On a device that runs unattended this is the main reason a fault goes
unnoticed. `screen` is the only supervisor.

### 3. Hardcoded MAC address

`TARGET_MAC` in `src/main.rs` holds `A5:C2:37:02:49:87`. No argument or environment
variable overrides it, so a second battery needs a rebuild.

### 4. The regulation mode is undecoded

`STATUS?` bit 0 is said to separate constant-voltage from constant-current. Nothing
measured on this unit identifies which way round it works, so `OutputStatus.raw` carries
the byte and the display does not claim CC or CV. Confirm against the Tenma protocol
document before printing it.

### 5. What the status frame holds is only partly named

`src/decode.rs` reads the status answer, and `README.md` lists the offsets. Three fields are
still unnamed: data offsets 10, 16, and 22. Offset 30 repeats the amp-hours left, which
suggests the tail is a second copy of part of the head. Cell voltages are not read at all:
they come from the command `0x04` answer, which nothing asks for, so per-cell balance is
invisible. There is no `Decode error` path any more and none is needed: a frame that does
not add up becomes `Outcome::Corrupt`, which is counted and printed.

## Suggested Order of Work

1. Ask command `0x04` and decode the cells. Per-cell spread is the reading that says
   whether a pack is well, and it is one query away.
2. Add timestamps to the output, and a way to restart the tool after a crash that does not
   need `/etc` writes.
3. Move `TARGET_MAC` to a command-line argument.
4. Print the mosfet state, which is decoded and thrown away. It is the reading that says
   whether the charge path is open at all, which is what the removed current comparison was
   trying to infer.
5. Name the three unnamed fields in the status answer. The pack is at 5 C in a cold garage
   while taking 3 A; a low-temperature charge limit belongs in the charge loop if any of
   those fields turns out to be a second probe.
