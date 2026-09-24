//! DSMR / e-MUCS P1 telegram parsing: CRC16 validation and OBIS extraction.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DsmrError {
    #[error("telegram missing start '/' or end '!' marker")]
    MissingMarkers,
    #[error("CRC mismatch: computed {computed:04X}, telegram says {declared:04X}")]
    CrcMismatch { computed: u16, declared: u16 },
    #[error("malformed CRC trailer")]
    MalformedCrc,
    #[error("missing OBIS code {0}")]
    MissingObis(&'static str),
    #[error("malformed OBIS value for {0}")]
    MalformedObis(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Telegram {
    pub import_w: f64,
    pub export_w: f64,
}

const OBIS_IMPORT: &str = "1-0:1.7.0";
const OBIS_EXPORT: &str = "1-0:2.7.0";

/// CRC16/ARC (poly 0xA001, init 0x0000, no final xor), as used by DSMR.
fn crc16_arc(data: &[u8]) -> u16 {
    let mut crc: u16 = 0x0000;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// Validates the CRC and parses import/export active power from a raw telegram.
///
/// The telegram must include the leading `/` and the trailing `!XXXX` CRC line.
pub fn parse_telegram(raw: &str) -> Result<Telegram, DsmrError> {
    let start = raw.find('/').ok_or(DsmrError::MissingMarkers)?;
    let bang = raw.rfind('!').ok_or(DsmrError::MissingMarkers)?;
    if bang < start {
        return Err(DsmrError::MissingMarkers);
    }

    // CRC covers everything from '/' through '!' inclusive.
    let crc_region = &raw[start..=bang];
    let computed = crc16_arc(crc_region.as_bytes());

    let trailer = &raw[bang + 1..];
    let hex: String = trailer.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    if hex.is_empty() {
        return Err(DsmrError::MalformedCrc);
    }
    let declared = u16::from_str_radix(hex.trim(), 16).map_err(|_| DsmrError::MalformedCrc)?;
    if declared != computed {
        return Err(DsmrError::CrcMismatch { computed, declared });
    }

    let import_w = extract_obis_kw(raw, OBIS_IMPORT)? * 1000.0;
    let export_w = extract_obis_kw(raw, OBIS_EXPORT)? * 1000.0;

    Ok(Telegram { import_w, export_w })
}

/// Extracts a value like `1-0:1.7.0(00123.456*kW)` and returns the numeric kW value.
fn extract_obis_kw(raw: &str, code: &'static str) -> Result<f64, DsmrError> {
    let line_start = raw.find(code).ok_or(DsmrError::MissingObis(code))?;
    let rest = &raw[line_start + code.len()..];
    let open = rest.find('(').ok_or(DsmrError::MalformedObis(code))?;
    let close = rest.find(')').ok_or(DsmrError::MalformedObis(code))?;
    if close < open {
        return Err(DsmrError::MalformedObis(code));
    }
    let inner = &rest[open + 1..close];
    let value_part = inner.split('*').next().unwrap_or("");
    value_part
        .trim()
        .parse::<f64>()
        .map_err(|_| DsmrError::MalformedObis(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Terminates a telegram body with `!` and its CRC.
    fn build_telegram(body: &str) -> String {
        let with_bang = format!("{body}!");
        let crc = crc16_arc(with_bang.as_bytes());
        format!("{with_bang}{crc:04X}\r\n")
    }

    #[test]
    fn parses_known_good_telegram() {
        let body = "/XMX5LGBBFFB123456789\r\n\r\n\
1-3:0.2.8(50)\r\n\
0-0:1.0.0(230101120000W)\r\n\
1-0:1.7.0(00.421*kW)\r\n\
1-0:2.7.0(01.234*kW)\r\n";
        let telegram = build_telegram(body);
        let parsed = parse_telegram(&telegram).expect("should parse");
        assert!((parsed.import_w - 421.0).abs() < 1e-6);
        assert!((parsed.export_w - 1234.0).abs() < 1e-6);
    }

    #[test]
    fn rejects_corrupted_crc() {
        let body = "/XMX5LGBBFFB123456789\r\n\r\n\
1-0:1.7.0(00.421*kW)\r\n\
1-0:2.7.0(01.234*kW)\r\n";
        let mut telegram = build_telegram(body);
        // Flip a byte in the payload without updating the trailing CRC.
        telegram.replace_range(30..31, "9");
        let err = parse_telegram(&telegram).unwrap_err();
        assert!(matches!(err, DsmrError::CrcMismatch { .. }));
    }

    #[test]
    fn rejects_missing_markers() {
        assert_eq!(parse_telegram("garbage"), Err(DsmrError::MissingMarkers));
    }

    #[test]
    fn rejects_missing_obis_code() {
        let body = "/XMX5LGBBFFB123456789\r\n1-0:2.7.0(01.234*kW)\r\n";
        let telegram = build_telegram(body);
        let err = parse_telegram(&telegram).unwrap_err();
        assert_eq!(err, DsmrError::MissingObis(OBIS_IMPORT));
    }
}
