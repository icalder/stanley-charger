# Project State

Inspection date: 2026-11-14. Working tree clean. Branch `main` is four commits ahead of
`origin/main`, and nothing is pushed yet.

## Summary

The tool reads battery data from a JBD BMS over BLE. It sets voltage and current on a
Tenma programmable power supply. It switches the supply output off when the battery
reaches the SoC limit. The tool does what the README claims.

The code is close to complete for the current single-battery setup. It has no tests. The
README now documents both flags, the charging phases, and the failure behaviour. The
reconnect logic has no path that leaves the supply on with no controller.

## Code Map

793 lines total. `cargo clippy --all-targets` reports 3 warnings and no errors. All three
are in `src/decode.rs`. `cargo fmt --check` is clean.

| File | Lines | State |
| --- | --- | --- |
| `src/main.rs` | 172 | Scans, connects, and subscribes to the BMS. Polls `QUERY_BATTERY` every 10 s. Restarts the monitor loop on error. Starts the power-supply task and exits when that task ends. |
| `src/decode.rs` | 165 | Decodes the 3-packet BMS response with `binary_layout`. Produces voltage, current, Ah, cycles, SoC, temperature, and 4 cell voltages in mV. |
| `src/tenma.rs` | 403 | Supervises the supply by USB `VID:PID` `0416:5011`. `connect_psu` waits for the device and opens it with the output off. `run_control_loop` writes `VSET1`, `ISET1`, and `OUT`, and reads `VOUT1?`. Returns a `ControlOutcome` that tells the supervisor whether to reopen the port or stop. |
| `src/charging.rs` | 53 | Calculates the charge phase from battery voltage. Pure function with no I/O. Every setpoint and boundary is a named constant. |

## Charging Algorithm

- BULK, below 13.6 V: target 16.00 V and 3.0 A.
- TAPER, 13.6 V to 14.2 V: target 14.40 V. Current falls from 3.0 A to 0.1 A.
- FLOAT, at or above 14.2 V: target 13.60 V and 0.1 A.

`MAX_CHARGE_CURRENT` sets the BULK current and the top of the TAPER ramp. `TAPER_SLOPE`
derives from that current, `FLOAT_CURRENT`, and the voltage window. The ramp lands on the
FLOAT current at the top of the window. Earlier code hardcoded a slope of 4.5 amps per
volt. That left TAPER at 0.5 A, so the current stepped down at the FLOAT boundary.

`--max-current` clamps the target current. `--limit` stops charging after 3 readings at
or above the limit, which takes about 30 s at the 10 s poll rate.

## PSU Link Handling

`run_psu_task` owns the retry loop. `run_control_loop` owns one port handle and returns
one of four `ControlOutcome` values. Nothing below the supervisor can end the task.

- `OutputOff`: the battery reached the limit and an `OUT0` write landed. The task exits.
- `OutputOffPending`: the battery reached the limit but no `OUT0` write landed. The
  supervisor sets `stop_pending`, reopens the port, and exits after that open writes
  `OUT0`. It never re-enters the control loop, so charging cannot resume.
- `LinkLost`: a write failed, the supply stopped answering the liveness check, or it
  stayed silent for `NO_RESPONSE_BUDGET` (30 s). The supervisor drops the handle and
  reconnects after 2 s.
- `ChannelClosed`: the BMS sender went away. The task exits.

Two timers drive the detection. `NO_RESPONSE_BUDGET` (30 s) watches `VOUT1?` answers, and
the control-mode liveness probe queries even while the setpoints change.
`BATTERY_STALE_BUDGET` (90 s) watches the battery channel. On expiry the tool switches the
output off, waits, and reprograms the setpoints when data returns. `connect_psu` retries
forever, so an unplugged supply pauses charging instead of ending it.

## Build and Deploy

The `Makefile` runs `build`, `patch`, then `deploy`. `patch` sets the interpreter to
`/lib/ld-linux-armhf.so.3` for the Cerbo GX. `deploy` copies the binary to
`root@einstein`. Run `make deploy` before the next test on the Cerbo. It rebuilds, so the
deployed binary always matches the source tree.

## Gaps

Ranked by risk to correct charging behaviour.

### 1. No tests

`calculate_charging_parameters` and `Decoder::decode` are pure functions. Neither has a
test. The phase boundaries at 13.6 V and 14.2 V and the taper slope are the places where
a silent regression would overcharge the battery. `src/tenma.rs` has no test either, so
the `ControlOutcome` state machine above is only checked by reading it.

### 2. Ctrl+C leaves the supply on

There is no signal handler. `Ctrl+C` kills the process and the supply keeps its last
setpoint with the output on. The README tells the operator to switch the output off first.
A `tokio::signal::ctrl_c` handler that sends `OUT0` would close this.

### 3. Hardcoded MAC address

`TARGET_MAC` in `src/main.rs` holds `A5:C2:37:02:49:87`. No argument or environment
variable overrides it. A second battery needs a rebuild.

### 4. Decoder is incomplete

Three `TODO` markers remain in `src/decode.rs`. The decoder reads only 4 cells. It ignores
the rest of packet 2. `decode` returns `Result`, but a commented-out error path means the
`Decode error:` branch in `run_monitor_loop` can never run.

### 5. No logging or service manager

The tool uses `println!` and `eprintln!`. The output has no timestamps and no rotation,
which is a problem on a device that runs unattended. The README starts it under `screen`,
so nothing restarts the process after a crash.

### 6. Lint warnings

3 warnings remain, all in `src/decode.rs`: `match_like_matches_macro` in `is_soc_ready`, a
derivable `impl`, and `&Vec` where `&[_]` is enough.

## Suggested Order of Work

1. Add tests for `calculate_charging_parameters` and `Decoder::decode`. Cover both phase
   boundaries and the taper slope.
2. Add a `ctrl_c` handler that switches the output off before the process exits.
3. Move `TARGET_MAC` to a command-line argument.
4. Replace `println!` with a logger that adds timestamps.
5. Clear the 3 clippy warnings in `src/decode.rs`.
