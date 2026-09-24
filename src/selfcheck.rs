//! Self-check run on every connection: reads back the comm-loss fallback
//! configuration and operating mode, and by default refuses to arm control
//! if they are unsafe.

use thiserror::Error;
use tracing::{error, info, warn};

use crate::config::{Config, SetpointEncoding};
use crate::modbus_client::{InverterLink, ModbusIoError};

/// SMA's NaN for a signed 32-bit register (0x8000 0000).
const S32_NAN: f64 = i32::MIN as f64;

/// `Inverter.WModCfg.WCtlComCfg.WCtlComAct` "Active" code. Setpoint writes
/// are ignored without it, whatever the operating mode.
const COMM_CONTROL_ACTIVE: u32 = 802;

/// Fallback mode "apply fallback values" code. The other code, 2506
/// ("values maintained"), holds the last setpoint forever on comm loss.
const APPLY_FALLBACK_VALUES: f64 = 2507.0;

/// `Inverter.WModCfg.WMod` "External active power setpoint" code. Other
/// codes: 303 (off), 1077 (manual W), 1078 (manual %).
const EXTERNAL_SETPOINT_MODE: f64 = 1079.0;

const MAX_SAFE_TIMEOUT_S: f64 = 30.0;

#[derive(Debug, Error)]
pub enum SelfCheckError {
    #[error("modbus error: {0}")]
    Modbus(#[from] ModbusIoError),
    #[error("self-check found {} issue(s): {}", .0.len(), .0.join("; "))]
    FailedChecks(Vec<String>),
}

pub struct SelfCheckReport {
    pub issues: Vec<String>,
}

/// Checks register values against the safety criteria. A `None` register
/// could not be read; the caller warns about it, so it is not an issue.
fn evaluate(
    mode: f64,
    timeout_s: f64,
    fallback_value_w: Option<f64>,
    operating_mode: Option<f64>,
    fallback_max_w: f64,
) -> Vec<String> {
    let mut issues = Vec::new();

    if let Some(op_mode) = operating_mode {
        if op_mode != EXTERNAL_SETPOINT_MODE {
            issues.push(format!(
                "operating mode active power setting is {op_mode:.0}, expected \
                 {EXTERNAL_SETPOINT_MODE:.0} (\"External active power setpoint\"); the inverter \
                 is ignoring this daemon's setpoint writes"
            ));
        }
    }

    if mode != APPLY_FALLBACK_VALUES {
        issues.push(format!(
            "fallback mode is {mode:.0}, expected {APPLY_FALLBACK_VALUES:.0} (\"apply fallback \
             values\"); on comm loss the inverter would hold its last setpoint instead of \
             falling back"
        ));
    }

    if timeout_s <= 0.0 || timeout_s > MAX_SAFE_TIMEOUT_S {
        issues.push(format!(
            "fallback timeout is {timeout_s:.0}s, outside the safe range (0, {MAX_SAFE_TIMEOUT_S:.0}]s"
        ));
    }

    match fallback_value_w {
        Some(w) if w <= S32_NAN + 1.0 => {
            issues.push(
                "fallback value register read back as NaN (never configured); confirm manually \
                 in the inverter's UI that it is set at or below FALLBACK_MAX_W"
                    .to_string(),
            );
        }
        Some(w) if w > fallback_max_w => {
            issues.push(format!(
                "fallback value is {w:.0} W, which exceeds FALLBACK_MAX_W ({fallback_max_w:.0} W)"
            ));
        }
        Some(_) | None => {}
    }

    issues
}

/// Converts a percentage of `Inverter.WMax` to watts.
fn percent_of_wmax_to_watts(fallback_pct: f64, wmax_w: f64) -> f64 {
    wmax_w * (fallback_pct / 100.0)
}

/// Whether `issues` block arming.
fn decide(issues: Vec<String>, enforce: bool) -> Result<(), SelfCheckError> {
    if issues.is_empty() || !enforce {
        Ok(())
    } else {
        Err(SelfCheckError::FailedChecks(issues))
    }
}

/// Reads back the fallback and operating mode registers and evaluates them.
/// Failing to read the fallback value or operating mode only warns.
pub async fn check_fallback_safe(
    link: &mut InverterLink,
    cfg: &Config,
    wmax_w: f64,
) -> Result<SelfCheckReport, ModbusIoError> {
    let mode = link.read_scaled(&cfg.fallback_mode_reg).await?;
    let timeout_s = link.read_scaled(&cfg.fallback_timeout_reg).await?;
    info!(fallback_mode_raw = mode, fallback_timeout_s = timeout_s, "read back comm-loss fallback registers");

    let fallback_value_w = match link.read_scaled(&cfg.fallback_value_reg).await {
        Ok(raw) if cfg.fallback_value_encoding == SetpointEncoding::Percent => {
            let w = percent_of_wmax_to_watts(raw, wmax_w);
            info!(fallback_value_pct = raw, inverter_wmax_w = wmax_w, fallback_value_w = w, "read back comm-loss fallback value");
            Some(w)
        }
        Ok(w) => {
            info!(fallback_value_w = w, "read back comm-loss fallback value");
            Some(w)
        }
        Err(e) => {
            warn!(error = %e, "could not read back fallback value register; \
                   skipping this check, verify manually");
            None
        }
    };

    let operating_mode = match link.read_scaled(&cfg.operating_mode_reg).await {
        Ok(m) => {
            info!(operating_mode_raw = m, "read back operating mode active power setting");
            Some(m)
        }
        Err(e) => {
            warn!(error = %e, "could not read back operating mode register; \
                   skipping this check, verify manually that it is set to External active power setpoint");
            None
        }
    };

    Ok(SelfCheckReport {
        issues: evaluate(mode, timeout_s, fallback_value_w, operating_mode, cfg.fallback_max_w),
    })
}

/// Activates control via communication if configured, then runs the
/// self-check. Fails on issues unless `SELFCHECK_ENFORCE=false`.
pub async fn arm(
    link: &mut InverterLink,
    cfg: &Config,
    wmax_w: f64,
) -> Result<(), SelfCheckError> {
    if cfg.comm_control_activate {
        if cfg.dry_run {
            info!(
                addr = cfg.comm_control_activate_addr,
                value = COMM_CONTROL_ACTIVE,
                "DRY_RUN: would activate active/reactive power control via communication (WCtlComAct)"
            );
        } else {
            link.write_u32(cfg.comm_control_activate_addr, COMM_CONTROL_ACTIVE).await?;
            info!("activated active/reactive power control via communication (WCtlComAct)");
        }
    }

    let report = check_fallback_safe(link, cfg, wmax_w).await?;

    if report.issues.is_empty() {
        info!("self-check passed: comm-loss fallback is configured safely, control armed");
        return Ok(());
    }

    for issue in &report.issues {
        error!(issue, "self-check finding");
    }

    if !cfg.selfcheck_enforce {
        warn!(
            "SELFCHECK_ENFORCE=false: arming despite {} failed self-check finding(s) above -- \
             this may be unsafe",
            report.issues.len()
        );
    } else {
        error!("refusing to arm control due to failed self-check (set SELFCHECK_ENFORCE=false to override)");
    }

    decide(report.issues, cfg.selfcheck_enforce)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: f64 = 2500.0;
    const EXTERNAL_MODE: Option<f64> = Some(1079.0);

    #[test]
    fn safe_configuration_has_no_issues() {
        let issues = evaluate(2507.0, 5.0, Some(2000.0), EXTERNAL_MODE, LIMIT);
        assert!(issues.is_empty());
    }

    #[test]
    fn wrong_mode_is_an_issue() {
        let issues = evaluate(2506.0, 5.0, Some(2000.0), EXTERNAL_MODE, LIMIT);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("fallback mode"));
    }

    #[test]
    fn zero_timeout_is_an_issue() {
        let issues = evaluate(2507.0, 0.0, Some(2000.0), EXTERNAL_MODE, LIMIT);
        assert!(issues.iter().any(|i| i.contains("timeout")));
    }

    #[test]
    fn timeout_above_max_is_an_issue() {
        let issues = evaluate(2507.0, 600.0, Some(2000.0), EXTERNAL_MODE, LIMIT);
        assert!(issues.iter().any(|i| i.contains("timeout")));
    }

    #[test]
    fn fallback_value_above_limit_is_an_issue() {
        let issues = evaluate(2507.0, 5.0, Some(3000.0), EXTERNAL_MODE, LIMIT);
        assert!(issues.iter().any(|i| i.contains("exceeds FALLBACK_MAX_W")));
    }

    #[test]
    fn fallback_value_at_exactly_the_limit_is_safe() {
        let issues = evaluate(2507.0, 5.0, Some(LIMIT), EXTERNAL_MODE, LIMIT);
        assert!(issues.is_empty());
    }

    #[test]
    fn nan_fallback_value_is_an_issue() {
        let issues = evaluate(2507.0, 5.0, Some(i32::MIN as f64), EXTERNAL_MODE, LIMIT);
        assert!(issues.iter().any(|i| i.contains("NaN")));
    }

    #[test]
    fn unreadable_fallback_value_is_not_an_issue_on_its_own() {
        let issues = evaluate(2507.0, 5.0, None, EXTERNAL_MODE, LIMIT);
        assert!(issues.is_empty());
    }

    #[test]
    fn multiple_problems_all_get_reported() {
        let issues = evaluate(2506.0, 600.0, Some(3000.0), EXTERNAL_MODE, LIMIT);
        assert_eq!(issues.len(), 3);
    }

    #[test]
    fn wrong_operating_mode_is_an_issue() {
        // 1078: manual %.
        let issues = evaluate(2507.0, 5.0, Some(2000.0), Some(1078.0), LIMIT);
        assert!(issues.iter().any(|i| i.contains("operating mode")));
    }

    #[test]
    fn unreadable_operating_mode_is_not_an_issue_on_its_own() {
        let issues = evaluate(2507.0, 5.0, Some(2000.0), None, LIMIT);
        assert!(issues.is_empty());
    }

    #[test]
    fn decide_passes_with_no_issues_regardless_of_enforce() {
        assert!(decide(vec![], true).is_ok());
        assert!(decide(vec![], false).is_ok());
    }

    #[test]
    fn decide_blocks_on_issues_when_enforced() {
        assert!(decide(vec!["bad".to_string()], true).is_err());
    }

    #[test]
    fn decide_allows_override_when_not_enforced() {
        assert!(decide(vec!["bad".to_string()], false).is_ok());
    }

    #[test]
    fn percent_of_wmax_basic() {
        assert_eq!(percent_of_wmax_to_watts(50.0, 4000.0), 2000.0);
    }

    #[test]
    fn percent_of_wmax_full_scale() {
        assert_eq!(percent_of_wmax_to_watts(100.0, 4000.0), 4000.0);
    }

    #[test]
    fn percent_of_wmax_zero() {
        assert_eq!(percent_of_wmax_to_watts(0.0, 4000.0), 0.0);
    }

    #[test]
    fn end_to_end_evaluate_flags_fallback_above_limit() {
        // 65 % of 4000 W is 2600 W, above a 2500 W maximum.
        let fallback_w = percent_of_wmax_to_watts(65.0, 4000.0);
        assert_eq!(fallback_w, 2600.0);
        let issues = evaluate(2507.0, 5.0, Some(fallback_w), EXTERNAL_MODE, 2500.0);
        assert!(issues.iter().any(|i| i.contains("exceeds FALLBACK_MAX_W")));
    }

    #[test]
    fn fallback_above_export_limit_accepted_up_to_fallback_max() {
        // Accepted even though above the 2500 W export limit.
        let fallback_w = percent_of_wmax_to_watts(65.0, 4000.0);
        let issues = evaluate(2507.0, 5.0, Some(fallback_w), EXTERNAL_MODE, 2600.0);
        assert!(issues.is_empty());
    }
}
