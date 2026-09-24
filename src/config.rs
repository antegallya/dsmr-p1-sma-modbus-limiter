//! Configuration from environment variables. Register defaults must be
//! checked against the device's own SMA Modbus register list.

use std::env;
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterType {
    U16,
    S16,
    U32,
    S32,
}

impl RegisterType {
    pub fn word_count(self) -> u16 {
        match self {
            RegisterType::U16 | RegisterType::S16 => 1,
            RegisterType::U32 | RegisterType::S32 => 2,
        }
    }
}

impl std::str::FromStr for RegisterType {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_uppercase().as_str() {
            "U16" => Ok(RegisterType::U16),
            "S16" => Ok(RegisterType::S16),
            "U32" => Ok(RegisterType::U32),
            "S32" => Ok(RegisterType::S32),
            other => Err(ConfigError::Invalid(format!("unknown register type '{other}'"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetpointEncoding {
    Watts,
    Percent,
}

impl std::str::FromStr for SetpointEncoding {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_uppercase().as_str() {
            "W" | "WATTS" => Ok(SetpointEncoding::Watts),
            "PCT" | "PERCENT" | "%" => Ok(SetpointEncoding::Percent),
            other => Err(ConfigError::Invalid(format!("unknown setpoint encoding '{other}'"))),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Missing(&'static str),
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Missing(name) => write!(f, "missing required env var {name}"),
            ConfigError::Invalid(msg) => write!(f, "invalid config: {msg}"),
        }
    }
}
impl std::error::Error for ConfigError {}

#[derive(Debug, Clone)]
pub struct RegisterSpec {
    pub address: u16,
    pub reg_type: RegisterType,
    pub scale: f64,
}

#[derive(Debug, Clone)]
pub struct Config {
    // E20 (DSMR source).
    pub e20_host: String,
    pub e20_port: u16,

    // Inverter Modbus TCP.
    pub inverter_host: String,
    pub inverter_port: u16,
    pub inverter_unit_id_ctrl: u8,

    // Limits.
    pub export_limit_w: f64,
    // Highest fallback power the self-check accepts.
    pub fallback_max_w: f64,

    // Registers.
    pub ac_power_reg: RegisterSpec,
    pub setpoint_reg: RegisterSpec,
    pub setpoint_encoding: SetpointEncoding,
    pub fallback_mode_reg: RegisterSpec,
    pub fallback_timeout_reg: RegisterSpec,
    pub fallback_value_reg: RegisterSpec,
    pub fallback_value_encoding: SetpointEncoding,
    // Inverter.WMax: setpoint upper clamp and base of percent encodings.
    pub inverter_wmax_reg: RegisterSpec,
    // Read-only: operating mode (self-check), current limit (debug log).
    pub operating_mode_reg: RegisterSpec,
    pub current_limit_reg: RegisterSpec,

    // Inverter.WModCfg.WCtlComCfg.WCtlComAct: setpoint writes are ignored
    // unless set to 802 (Active). Write-only, so written on every arm.
    pub comm_control_activate_addr: u16,
    pub comm_control_activate: bool,

    // Refuse to arm when the self-check reports issues.
    pub selfcheck_enforce: bool,

    // Keep writing setpoints (with AC power taken as 0 W) while the AC power
    // register reads as the "not implemented" sentinel, i.e. while the
    // inverter isn't producing. When false, those cycles write nothing.
    pub write_when_not_producing: bool,

    // Timing.
    pub telegram_staleness: Duration,
    pub reconnect_backoff_min: Duration,
    pub reconnect_backoff_max: Duration,

    // Safety / ops.
    pub dry_run: bool,
    pub log_level: String,
    pub health_port: u16,
}

fn env_str(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> Result<T, ConfigError>
where
    T::Err: fmt::Display,
{
    match env::var(name) {
        Ok(v) => v.parse::<T>().map_err(|e| ConfigError::Invalid(format!("{name}: {e}"))),
        Err(_) => Ok(default),
    }
}

fn env_bool(name: &str, default: bool) -> Result<bool, ConfigError> {
    match env::var(name) {
        Ok(v) => match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            other => Err(ConfigError::Invalid(format!("{name}: not a bool: '{other}'"))),
        },
        Err(_) => Ok(default),
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let e20_host = env::var("E20_HOST").map_err(|_| ConfigError::Missing("E20_HOST"))?;
        let inverter_host =
            env::var("INVERTER_HOST").map_err(|_| ConfigError::Missing("INVERTER_HOST"))?;

        // Defaults to true so that a first run never writes.
        let dry_run = env_bool("DRY_RUN", true)?;

        let ac_power_reg = RegisterSpec {
            address: env_parse("AC_POWER_ADDR", 30775u16)?,
            reg_type: env_parse("AC_POWER_TYPE", RegisterType::S32)?,
            scale: env_parse("AC_POWER_SCALE", 1.0f64)?,
        };

        let setpoint_encoding: SetpointEncoding = env_parse("SETPOINT_ENCODING", SetpointEncoding::Percent)?;
        // No default: the setpoint register must be checked per device.
        let setpoint_addr_str =
            env::var("SETPOINT_ADDR").map_err(|_| ConfigError::Missing("SETPOINT_ADDR"))?;
        let setpoint_reg = RegisterSpec {
            address: setpoint_addr_str
                .parse::<u16>()
                .map_err(|e| ConfigError::Invalid(format!("SETPOINT_ADDR: {e}")))?,
            reg_type: env_parse("SETPOINT_TYPE", RegisterType::S32)?,
            scale: env_parse("SETPOINT_SCALE", 1.0f64)?,
        };

        let export_limit_w = env_parse("EXPORT_LIMIT_W", 2500.0f64)?;

        let staleness_ms: u64 = env_parse("TELEGRAM_STALENESS_MS", 3000u64)?;
        let backoff_min_ms: u64 = env_parse("RECONNECT_BACKOFF_MIN_MS", 500u64)?;
        let backoff_max_ms: u64 = env_parse("RECONNECT_BACKOFF_MAX_MS", 30_000u64)?;

        Ok(Config {
            e20_host,
            e20_port: env_parse("E20_PORT", 23u16)?,
            inverter_host,
            inverter_port: env_parse("INVERTER_PORT", 502u16)?,
            inverter_unit_id_ctrl: env_parse("INVERTER_UNIT_ID_CTRL", 3u8)?,
            export_limit_w,
            fallback_max_w: env_parse("FALLBACK_MAX_W", export_limit_w)?,
            ac_power_reg,
            setpoint_reg,
            setpoint_encoding,
            // Per the SMA Modbus register list for SBxx-1AV-41 firmware
            // 4.01.15.R, pre-2018 country dataset. 41197 is FlbWNom, a
            // percentage of Inverter.WMax.
            fallback_mode_reg: RegisterSpec {
                address: env_parse("FALLBACK_MODE_ADDR", 41193u16)?,
                reg_type: env_parse("FALLBACK_MODE_TYPE", RegisterType::U32)?,
                scale: env_parse("FALLBACK_MODE_SCALE", 1.0f64)?,
            },
            fallback_timeout_reg: RegisterSpec {
                address: env_parse("FALLBACK_TIMEOUT_ADDR", 41195u16)?,
                reg_type: env_parse("FALLBACK_TIMEOUT_TYPE", RegisterType::U32)?,
                scale: env_parse("FALLBACK_TIMEOUT_SCALE", 1.0f64)?,
            },
            fallback_value_reg: RegisterSpec {
                address: env_parse("FALLBACK_VALUE_ADDR", 41197u16)?,
                reg_type: env_parse("FALLBACK_VALUE_TYPE", RegisterType::U32)?,
                scale: env_parse("FALLBACK_VALUE_SCALE", 0.01f64)?,
            },
            fallback_value_encoding: env_parse("FALLBACK_VALUE_ENCODING", SetpointEncoding::Percent)?,
            inverter_wmax_reg: RegisterSpec {
                address: env_parse("INVERTER_WMAX_ADDR", 30233u16)?,
                reg_type: env_parse("INVERTER_WMAX_TYPE", RegisterType::U32)?,
                scale: env_parse("INVERTER_WMAX_SCALE", 1.0f64)?,
            },
            // Inverter.WModCfg.WMod: expect 1079 ("External active power
            // setpoint").
            operating_mode_reg: RegisterSpec {
                address: env_parse("OPERATING_MODE_ADDR", 30835u16)?,
                reg_type: env_parse("OPERATING_MODE_TYPE", RegisterType::U32)?,
                scale: env_parse("OPERATING_MODE_SCALE", 1.0f64)?,
            },
            // Operation.Dmd.WCtl: the limit currently enforced.
            current_limit_reg: RegisterSpec {
                address: env_parse("CURRENT_LIMIT_ADDR", 31405u16)?,
                reg_type: env_parse("CURRENT_LIMIT_TYPE", RegisterType::U32)?,
                scale: env_parse("CURRENT_LIMIT_SCALE", 1.0f64)?,
            },
            comm_control_activate_addr: env_parse("COMM_CONTROL_ACTIVATE_ADDR", 40151u16)?,
            comm_control_activate: env_bool("COMM_CONTROL_ACTIVATE", true)?,
            selfcheck_enforce: env_bool("SELFCHECK_ENFORCE", true)?,
            write_when_not_producing: env_bool("WRITE_WHEN_NOT_PRODUCING", false)?,
            telegram_staleness: Duration::from_millis(staleness_ms),
            reconnect_backoff_min: Duration::from_millis(backoff_min_ms),
            reconnect_backoff_max: Duration::from_millis(backoff_max_ms),
            dry_run,
            log_level: env_str("LOG_LEVEL", "info"),
            health_port: env_parse("HEALTH_PORT", 8080u16)?,
        })
    }
}

