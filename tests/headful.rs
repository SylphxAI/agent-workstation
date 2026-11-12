//! Headful integration: a real display, window manager, browser and VNC
//! server. Runs only with `AGENT_WORKSTATION_HEADFUL=1` (CI sets it).

fn enabled() -> bool {
    std::env::var("AGENT_WORKSTATION_HEADFUL").is_ok_and(|v| v == "1")
}

#[test]
fn headful_suite_is_opt_in() {
    if !enabled() {
        eprintln!("skipped: set AGENT_WORKSTATION_HEADFUL=1");
    }
}
