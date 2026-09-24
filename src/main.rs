mod config;
mod control;
mod dsmr;
mod e20;
mod health;
mod modbus_client;
mod selfcheck;
mod state;

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use config::Config;
use control::ControlInput;
use dsmr::Telegram;
use modbus_client::{resolve_socket_addr, InverterLink, ModbusIoError};
use selfcheck::SelfCheckError;
use state::AppState;

/// Handles `--healthcheck`: a tiny synchronous HTTP client used by the
/// container `HEALTHCHECK` instruction, so `scratch` doesn't need curl/wget.
fn run_healthcheck_probe() -> ! {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let port: u16 = std::env::var("HEALTH_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(8080);
    let ok = TcpStream::connect(("127.0.0.1", port))
        .and_then(|mut stream| {
            stream.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
            let mut buf = [0u8; 32];
            let n = stream.read(&mut buf)?;
            Ok(buf[..n].starts_with(b"HTTP/1.1 200") || buf[..n].starts_with(b"HTTP/1.0 200"))
        })
        .unwrap_or(false);
    std::process::exit(if ok { 0 } else { 1 });
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--healthcheck") {
        run_healthcheck_probe();
    }

    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fatal: {e}");
            std::process::exit(1);
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(cfg.log_level.clone()))
        .json()
        .init();

    if cfg.dry_run {
        warn!("DRY_RUN=true: computing and logging every decision, sending NO writes");
    }

    let state = Arc::new(AppState::new(cfg.telegram_staleness));
    tokio::spawn(health::run(cfg.health_port, state.clone()));

    let (tx, rx) = mpsc::channel::<Telegram>(16);
    tokio::spawn(e20::run(
        cfg.e20_host.clone(),
        cfg.e20_port,
        tx,
        cfg.reconnect_backoff_min,
        cfg.reconnect_backoff_max,
    ));

    if let Err(e) = run_control_loop(cfg, rx, state).await {
        error!(error = %e, "exiting");
        std::process::exit(1);
    }
}

/// Connects, reads Inverter.WMax and arms. A failed WMax read fails the
/// whole connection: without it, no setpoint or percentage is meaningful.
async fn connect_and_arm(
    cfg: &Config,
) -> Result<(InverterLink, f64), SelfCheckError> {
    let addr = resolve_socket_addr(&cfg.inverter_host, cfg.inverter_port).await?;
    let mut link = InverterLink::connect(addr, cfg.inverter_unit_id_ctrl).await?;
    info!(host = %cfg.inverter_host, port = cfg.inverter_port, "connected to inverter");
    let wmax_w = link.read_scaled(&cfg.inverter_wmax_reg).await?;
    info!(inverter_wmax_w = wmax_w, "read Inverter.WMax");
    selfcheck::arm(&mut link, cfg, wmax_w).await?;
    Ok((link, wmax_w))
}

async fn write_setpoint(
    link: &mut InverterLink,
    cfg: &Config,
    wmax_w: f64,
    setpoint_w: f64,
) -> Result<(), modbus_client::ModbusIoError> {
    let raw = match cfg.setpoint_encoding {
        config::SetpointEncoding::Watts => {
            control::watts_to_scaled(setpoint_w, cfg.setpoint_reg.scale)
        }
        config::SetpointEncoding::Percent => {
            control::watts_to_percent_scaled(setpoint_w, wmax_w, cfg.setpoint_reg.scale)
        }
    };
    link.write_raw(&cfg.setpoint_reg, raw).await
}

/// Returns an error only when the self-check refuses to arm: that needs a
/// fix on the inverter, so the process exits.
async fn run_control_loop(
    cfg: Config,
    mut rx: mpsc::Receiver<Telegram>,
    state: Arc<AppState>,
) -> Result<(), SelfCheckError> {
    // Connected and armed.
    let mut inverter: Option<InverterLink> = None;
    // Inverter.WMax, valid whenever `inverter` is Some.
    let mut wmax_w = 0.0;
    let mut backoff = cfg.reconnect_backoff_min;
    let mut next_reconnect_attempt = Instant::now();
    let mut lapsed = true;

    loop {
        let recv_result = tokio::time::timeout(cfg.telegram_staleness, rx.recv()).await;

        let telegram = match recv_result {
            Err(_elapsed) => {
                if !lapsed {
                    warn!("no fresh telegram within staleness window; lapsing (writes stopped)");
                    lapsed = true;
                }
                state.record_action();
                continue;
            }
            Ok(None) => {
                error!("E20 reader channel closed; shutting down control loop");
                return Ok(());
            }
            Ok(Some(t)) => t,
        };

        state.record_telegram();
        if lapsed {
            info!("fresh telegram received; resuming normal operation");
            lapsed = false;
        }

        if inverter.is_none() && Instant::now() >= next_reconnect_attempt {
            match connect_and_arm(&cfg).await {
                Ok((link, link_wmax_w)) => {
                    wmax_w = link_wmax_w;
                    backoff = cfg.reconnect_backoff_min;
                    inverter = Some(link);
                }
                Err(e @ SelfCheckError::FailedChecks(_)) => return Err(e),
                Err(SelfCheckError::Modbus(e)) => {
                    warn!(error = %e, backoff_ms = backoff.as_millis(), "inverter connect failed, retrying with backoff");
                    next_reconnect_attempt = Instant::now() + backoff;
                    backoff = (backoff * 2).min(cfg.reconnect_backoff_max);
                }
            }
        }

        let Some(link) = inverter.as_mut() else {
            state.record_action();
            continue;
        };

        let ac_power_w = match link.read_scaled(&cfg.ac_power_reg).await {
            Ok(v) => v,
            // The inverter reports AC power as the sentinel while the panels
            // are not producing (e.g. at night): that is 0 W, not a fault.
            Err(ModbusIoError::NotImplemented { .. }) => {
                if !cfg.write_when_not_producing {
                    debug!("AC power reads as sentinel; inverter not producing, skipping cycle");
                    state.record_action();
                    continue;
                }
                debug!("AC power reads as sentinel; inverter not producing, using 0 W");
                0.0
            }
            Err(e) => {
                error!(error = %e, "AC power read failed; lapsing and dropping inverter connection");
                inverter = None;
                next_reconnect_attempt = Instant::now();
                state.record_action();
                continue;
            }
        };

        let output = control::compute(ControlInput {
            import_w: telegram.import_w,
            export_w: telegram.export_w,
            inverter_ac_w: ac_power_w,
            export_limit_w: cfg.export_limit_w,
            inverter_wmax_w: wmax_w,
        });

        // Debug only: the limit currently enforced, to compare with
        // setpoint_w (live setpoint vs comm-loss fallback). Errors ignored.
        let current_limit_w = match link.read_scaled(&cfg.current_limit_reg).await {
            Ok(v) => Some(v),
            Err(e) => {
                warn!(error = %e, "could not read current limit register (debug-only, ignoring)");
                None
            }
        };

        info!(
            import_w = telegram.import_w,
            export_w = telegram.export_w,
            inverter_ac_w = ac_power_w,
            house_load_w = output.house_load_w,
            setpoint_w = output.setpoint_w,
            current_limit_w = ?current_limit_w,
            dry_run = cfg.dry_run,
            "cycle"
        );

        if cfg.dry_run {
            state.record_action();
            continue;
        }

        if let Err(e) = write_setpoint(link, &cfg, wmax_w, output.setpoint_w).await {
            error!(error = %e, "setpoint write failed; lapsing and dropping inverter connection");
            inverter = None;
            next_reconnect_attempt = Instant::now();
        }
        state.record_action();
    }
}
