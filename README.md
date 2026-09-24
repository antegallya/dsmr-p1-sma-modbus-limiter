# dsmr-sma-limiter

A small, fail-safe daemon that caps grid export from an SMA solar inverter to a configured limit.
It reads DSMR P1 telegrams and writes an active-power setpoint to the inverter over Modbus TCP.

Read "Prerequisites" and "Arming checklist" before setting `DRY_RUN=false`.

## How it works

Once per CRC-valid telegram:

1. Read `import_W` (OBIS `1-0:1.7.0`) and `export_W` (OBIS `1-0:2.7.0`).
2. Read the inverter's AC power.
3. `house_load_W = (import_W − export_W) + inverter_ac_W`.
4. `setpoint_W = clamp(house_load_W + EXPORT_LIMIT_W, 0, WMax)`, where `WMax` is register `30233`, read on every connect.
5. Write the setpoint to the inverter's volatile "Immediate Inverter Controls" register.

While the inverter isn't producing (e.g. at night), its AC power register reads as the "not implemented" sentinel.
That is not treated as an error: by default the cycle writes nothing and the inverter stays on its comm-loss fallback.
With `WRITE_WHEN_NOT_PRODUCING=true`, AC power is taken as 0 W and the setpoint is written as usual.

## Fail-safe design

- On any error (stale telegram, CRC failure, read or write failure, out-of-range value) the daemon stops writing.
  It never re-sends the last setpoint.
- The inverter's comm-loss fallback (registers `41193`/`41195`/`41197`) is the dead-man switch: without writes, the inverter reverts to a fixed low power on its own.
- At startup and after every reconnect, the daemon reads back the fallback configuration and exits if it isn't safe (timeout out of range, or fallback power above `FALLBACK_MAX_W`, default `EXPORT_LIMIT_W`).
- `DRY_RUN` defaults to `true`.

## Prerequisites

Configure these on the inverter; this program never writes EEPROM parameters.
Check register addresses against your device's register list.

- Modbus TCP server enabled.
- "Operating mode active power setpoint" set to **External setpoint** (register `30835`, checked by the self-check).
- Register `40151` set to `802` ("Active").
  Without it, the inverter silently ignores setpoint writes.
  The daemon writes it on every arm (`COMM_CONTROL_ACTIVATE`, default on), since it can't be read back.
- The inverter's own dynamic active-power limitation deactivated (it conflicts with Modbus control).
- Comm-loss fallback:
  - `41193` fallback mode set to `2507` ("apply fallback values"), not `2506` ("values maintained").
  - `41195` timeout, e.g. 5 s (default 600 s is too long).
  - `41197` fallback value, as a percentage of `WMax`, such that `41197 % * WMax <= FALLBACK_MAX_W`.
- Active-power gradient set to 100 %/s.

## Registers

All register addresses, types and scales are configurable.
Check them against your inverter's "SMA Modbus register list" before setting `DRY_RUN=false`.

- `SETPOINT_ADDR` has no default: it must be the volatile setpoint register, not an EEPROM parameter.
- Set `SETPOINT_ENCODING` and `SETPOINT_SCALE` to match it (W or percentage of WMax).
  E.g. per the SBxx-1AV-41 register list: `40023`, 0.01 % of WMax (`SETPOINT_ENCODING=PCT`, `SETPOINT_SCALE=0.01`), or `40149`, watts.
- `AC_POWER_ADDR` defaults to `30775`, S32, scale 1.

`SELFCHECK_ENFORCE=false` lets the daemon arm even if the self-check fails (findings are still logged).
Use it only if you checked the fallback configuration by hand.

## Configuration

See `env.example`.
Copy it to `.env` and fill it in.

## Arming checklist

1. Complete every item under "Prerequisites".
2. Check every register value in `.env` against your device's register list.
3. Run with `DRY_RUN=true` and check that `import_w`, `export_w`, `inverter_ac_w`, `house_load_w` and `setpoint_w` look right with PV off, self-consumed and exporting.
4. Check that the startup log reports "self-check passed".
5. Set `DRY_RUN=false`.

## Observability

- JSON logs to stdout: one line per cycle, plus reconnects and fail-safe transitions.
  `current_limit_w` (register `31405`) is the limit the inverter is actually enforcing; if it differs from `setpoint_w`, the inverter is on its fallback.
- `GET /health` (default port `8080`) returns `200` only if a valid telegram and a write (or intentional lapse) happened recently.
  Used as the container `HEALTHCHECK`.

## Build & run

Native:

```sh
cargo build --release
cargo test
```

Container:

```sh
docker compose up --build
```
