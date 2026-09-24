//! Direct-algebra setpoint computation. No PID: corrected fresh every cycle.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlInput {
    pub import_w: f64,
    pub export_w: f64,
    pub inverter_ac_w: f64,
    pub export_limit_w: f64,
    pub inverter_wmax_w: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlOutput {
    pub house_load_w: f64,
    pub setpoint_w: f64,
}

/// House load is grid import minus export plus inverter AC power. The
/// setpoint is house load plus the export limit, clamped to [0, WMax].
pub fn compute(input: ControlInput) -> ControlOutput {
    let house_load_w = (input.import_w - input.export_w) + input.inverter_ac_w;
    let raw_setpoint = house_load_w + input.export_limit_w;
    let setpoint_w = raw_setpoint.clamp(0.0, input.inverter_wmax_w);
    ControlOutput { house_load_w, setpoint_w }
}

/// Converts watts to a raw percentage of Inverter.WMax, `scale` being the
/// register unit in percent.
pub fn watts_to_percent_scaled(setpoint_w: f64, wmax_w: f64, scale: f64) -> i32 {
    let pct = if wmax_w > 0.0 { (setpoint_w / wmax_w) * 100.0 } else { 0.0 };
    (pct / scale).round() as i32
}

/// Converts watts to a raw register value, `scale` being the register unit
/// in watts.
pub fn watts_to_scaled(setpoint_w: f64, scale: f64) -> i32 {
    (setpoint_w / scale).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    const WMAX: f64 = 4000.0;
    const LIMIT: f64 = 2500.0;

    fn input(import_w: f64, export_w: f64, inverter_ac_w: f64) -> ControlInput {
        ControlInput {
            import_w,
            export_w,
            inverter_ac_w,
            export_limit_w: LIMIT,
            inverter_wmax_w: WMAX,
        }
    }

    #[test]
    fn pv_off_house_load_equals_import() {
        // No PV: import 500 W, export 0, inverter output 0.
        let out = compute(input(500.0, 0.0, 0.0));
        assert_eq!(out.house_load_w, 500.0);
        assert_eq!(out.setpoint_w, 3000.0); // 500 + 2500, well under WMax.
    }

    #[test]
    fn pv_self_consumed_with_some_export() {
        // House uses 1000 W, PV makes 1500 W, 500 W exported.
        let out = compute(input(0.0, 500.0, 1500.0));
        assert_eq!(out.house_load_w, 1000.0);
        assert_eq!(out.setpoint_w, 3500.0); // 1000 + 2500
    }

    #[test]
    fn export_exactly_at_cap_holds_current_output() {
        // House load 0, PV fully exported and inverter already at WMax.
        let out = compute(input(0.0, 4000.0, 4000.0));
        assert_eq!(out.house_load_w, 0.0);
        assert_eq!(out.setpoint_w, 2500.0); // clamps to house_load + limit
    }

    #[test]
    fn zero_load_high_pv_clamped_to_limit() {
        let out = compute(input(0.0, 3900.0, 3900.0));
        assert_eq!(out.house_load_w, 0.0);
        assert_eq!(out.setpoint_w, 2500.0);
    }

    #[test]
    fn import_exceeding_pv_raises_setpoint_up_to_wmax_clamp() {
        let out = compute(input(3000.0, 0.0, 200.0));
        assert_eq!(out.house_load_w, 3200.0);
        // 3200 + 2500 = 5700, clamped to WMax.
        assert_eq!(out.setpoint_w, 4000.0);
    }

    #[test]
    fn setpoint_never_goes_negative() {
        // Negative house load (e.g. metering noise).
        let out = compute(ControlInput {
            import_w: 0.0,
            export_w: 5000.0,
            inverter_ac_w: 0.0,
            export_limit_w: -1000.0,
            inverter_wmax_w: WMAX,
        });
        assert!(out.setpoint_w >= 0.0);
    }

    #[test]
    fn watts_to_percent_scaled_basic() {
        // 2000 W of 4000 W WMax = 50%, scale 0.1 -> register value 500.
        let v = watts_to_percent_scaled(2000.0, 4000.0, 0.1);
        assert_eq!(v, 500);
    }

    #[test]
    fn watts_to_percent_scaled_unscaled() {
        let v = watts_to_percent_scaled(1000.0, 4000.0, 1.0);
        assert_eq!(v, 25);
    }

    #[test]
    fn watts_to_scaled_identity() {
        let v = watts_to_scaled(1234.0, 1.0);
        assert_eq!(v, 1234);
    }
}
