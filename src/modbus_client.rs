//! Thin wrapper around `tokio-modbus` for the single TCP connection to the
//! inverter, on the SMA-proprietary Unit ID (`INVERTER_UNIT_ID_CTRL`).

use std::net::SocketAddr;
use thiserror::Error;
use tokio::net::lookup_host;
use tokio_modbus::client::{tcp, Context};
use tokio_modbus::prelude::*;

use crate::config::{RegisterSpec, RegisterType};

#[derive(Debug, Error)]
pub enum ModbusIoError {
    #[error("DNS resolution failed for {host}: {source}")]
    Resolve { host: String, source: std::io::Error },
    #[error("host '{0}' resolved to no addresses")]
    NoAddress(String),
    #[error("connect error: {0}")]
    Connect(#[from] std::io::Error),
    #[error("transport error: {0}")]
    Transport(#[from] tokio_modbus::Error),
    #[error("device reported not armed (Modbus exception {0:?})")]
    NotArmed(ExceptionCode),
    #[error("device exception: {0:?}")]
    Exception(ExceptionCode),
    #[error("register {address} reads as SunSpec/SMA 'not implemented' sentinel ({raw:?}); value not available")]
    NotImplemented { address: u16, raw: i64 },
}

pub async fn resolve_socket_addr(host: &str, port: u16) -> Result<SocketAddr, ModbusIoError> {
    lookup_host((host, port))
        .await
        .map_err(|source| ModbusIoError::Resolve { host: host.to_string(), source })?
        .next()
        .ok_or_else(|| ModbusIoError::NoAddress(host.to_string()))
}

pub struct InverterLink {
    ctx: Context,
}

/// Exceptions 03 (IllegalDataValue) and 04 (ServerDeviceFailure) are taken
/// as a missing prerequisite, e.g. the wrong operating mode.
fn classify_exception(code: ExceptionCode) -> ModbusIoError {
    match code {
        ExceptionCode::IllegalDataValue | ExceptionCode::ServerDeviceFailure => {
            ModbusIoError::NotArmed(code)
        }
        other => ModbusIoError::Exception(other),
    }
}

fn unwrap_modbus_result<T>(r: tokio_modbus::Result<T>) -> Result<T, ModbusIoError> {
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(exc)) => Err(classify_exception(exc)),
        Err(io_err) => Err(ModbusIoError::Transport(io_err)),
    }
}

impl InverterLink {
    pub async fn connect(addr: SocketAddr, unit_id: u8) -> Result<Self, ModbusIoError> {
        let ctx = tcp::connect_slave(addr, Slave(unit_id)).await?;
        Ok(Self { ctx })
    }

    async fn read_raw(&mut self, spec: &RegisterSpec) -> Result<Vec<u16>, ModbusIoError> {
        let words = self
            .ctx
            .read_holding_registers(spec.address, spec.reg_type.word_count())
            .await;
        unwrap_modbus_result(words)
    }

    /// Reads a register and returns its scaled value. Fails on the "not
    /// implemented" sentinel.
    pub async fn read_scaled(&mut self, spec: &RegisterSpec) -> Result<f64, ModbusIoError> {
        let words = self.read_raw(spec).await?;
        let raw = decode_words(&words, spec.reg_type);
        if is_not_implemented_sentinel(raw, spec.reg_type) {
            return Err(ModbusIoError::NotImplemented { address: spec.address, raw });
        }
        Ok(raw as f64 * spec.scale)
    }

    /// Writes an already-scaled-to-raw integer value to a register.
    pub async fn write_raw(&mut self, spec: &RegisterSpec, raw: i32) -> Result<(), ModbusIoError> {
        let words = encode_words(raw, spec.reg_type);
        let result = self.ctx.write_multiple_registers(spec.address, &words).await;
        unwrap_modbus_result(result)
    }

    pub async fn write_u32(&mut self, addr: u16, value: u32) -> Result<(), ModbusIoError> {
        let words = [(value >> 16) as u16, (value & 0xFFFF) as u16];
        unwrap_modbus_result(self.ctx.write_multiple_registers(addr, &words).await)
    }
}

/// SunSpec/SMA "not implemented" value: the type's minimum if signed, all
/// ones if unsigned. Sent as a normal read, not as an exception.
fn is_not_implemented_sentinel(raw: i64, reg_type: RegisterType) -> bool {
    match reg_type {
        RegisterType::U16 => raw == u16::MAX as i64,
        RegisterType::S16 => raw == i16::MIN as i64,
        RegisterType::U32 => raw == u32::MAX as i64,
        RegisterType::S32 => raw == i32::MIN as i64,
    }
}

fn decode_words(words: &[u16], reg_type: RegisterType) -> i64 {
    match reg_type {
        RegisterType::U16 => words[0] as i64,
        RegisterType::S16 => (words[0] as i16) as i64,
        RegisterType::U32 => (((words[0] as u32) << 16) | words[1] as u32) as i64,
        RegisterType::S32 => ((((words[0] as u32) << 16) | words[1] as u32) as i32) as i64,
    }
}

fn encode_words(raw: i32, reg_type: RegisterType) -> Vec<u16> {
    match reg_type {
        RegisterType::U16 | RegisterType::S16 => vec![raw as u16],
        RegisterType::U32 | RegisterType::S32 => {
            let v = raw as u32;
            vec![(v >> 16) as u16, (v & 0xFFFF) as u16]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_s32_negative() {
        // -1 as S32 big-endian words: 0xFFFF, 0xFFFF.
        let words = [0xFFFFu16, 0xFFFFu16];
        assert_eq!(decode_words(&words, RegisterType::S32), -1);
    }

    #[test]
    fn decode_s32_positive() {
        // 70000 = 0x00011170
        let words = [0x0001u16, 0x1170u16];
        assert_eq!(decode_words(&words, RegisterType::S32), 70000);
    }

    #[test]
    fn encode_decode_roundtrip_s32() {
        let encoded = encode_words(-2500, RegisterType::S32);
        assert_eq!(decode_words(&encoded, RegisterType::S32), -2500);
    }

    #[test]
    fn decode_u16() {
        assert_eq!(decode_words(&[500], RegisterType::U16), 500);
    }

    #[test]
    fn decode_s16_negative() {
        assert_eq!(decode_words(&[0xFFFF], RegisterType::S16), -1);
    }

    #[test]
    fn exception_03_and_04_classified_as_not_armed() {
        assert!(matches!(
            classify_exception(ExceptionCode::IllegalDataValue),
            ModbusIoError::NotArmed(_)
        ));
        assert!(matches!(
            classify_exception(ExceptionCode::ServerDeviceFailure),
            ModbusIoError::NotArmed(_)
        ));
    }

    #[test]
    fn other_exceptions_not_classified_as_not_armed() {
        assert!(matches!(
            classify_exception(ExceptionCode::IllegalFunction),
            ModbusIoError::Exception(_)
        ));
    }

    #[test]
    fn s32_not_implemented_sentinel_detected() {
        // 0x80000000 as S32 is i32::MIN.
        let words = [0x8000u16, 0x0000u16];
        let raw = decode_words(&words, RegisterType::S32);
        assert_eq!(raw, i32::MIN as i64);
        assert!(is_not_implemented_sentinel(raw, RegisterType::S32));
    }

    #[test]
    fn u32_not_implemented_sentinel_detected() {
        let words = [0xFFFFu16, 0xFFFFu16];
        let raw = decode_words(&words, RegisterType::U32);
        assert_eq!(raw, u32::MAX as i64);
        assert!(is_not_implemented_sentinel(raw, RegisterType::U32));
    }

    #[test]
    fn s16_not_implemented_sentinel_detected() {
        let raw = decode_words(&[0x8000], RegisterType::S16);
        assert!(is_not_implemented_sentinel(raw, RegisterType::S16));
    }

    #[test]
    fn u16_not_implemented_sentinel_detected() {
        let raw = decode_words(&[0xFFFF], RegisterType::U16);
        assert!(is_not_implemented_sentinel(raw, RegisterType::U16));
    }

    #[test]
    fn ordinary_values_not_flagged_as_sentinel() {
        assert!(!is_not_implemented_sentinel(-1, RegisterType::S32));
        assert!(!is_not_implemented_sentinel(0, RegisterType::S32));
        assert!(!is_not_implemented_sentinel(i32::MAX as i64, RegisterType::S32));
        assert!(!is_not_implemented_sentinel(0, RegisterType::U32));
        assert!(!is_not_implemented_sentinel((u32::MAX - 1) as i64, RegisterType::U32));
    }
}
