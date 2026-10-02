//! Read-only probe: lists the audio sessions of a process tree with their endpoint and current
//! volume. It never changes any volume or mute state (only getters are called).
//!
//! Usage: cargo run -p devocal-core --example sessions_probe -- <pid>

#[cfg(windows)]
fn main() {
    use devocal_core::sessions::SessionVolumes;
    use devocal_core::sessions_win::{process_tree, WinSessions};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    let pid: u32 = match std::env::args().nth(1).and_then(|a| a.parse().ok()) {
        Some(p) => p,
        None => {
            eprintln!("usage: sessions_probe <pid>");
            std::process::exit(2);
        }
    };
    // WinSessions requires an MTA-initialised thread; the probe does it itself.
    if let Err(e) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        eprintln!("CoInitializeEx failed: {e}");
        std::process::exit(1);
    }

    let s = WinSessions;
    println!("process_created({pid}) = {:?}", s.process_created(pid));
    println!("process tree: {:?}", process_tree(pid));
    let sessions = match s.sessions_for_tree(pid) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("sessions_for_tree failed: {e}");
            std::process::exit(1);
        }
    };
    println!("{} session(s)", sessions.len());
    for info in &sessions {
        println!("- instance: {}", info.instance_id);
        println!("  pid: {}  active: {}", info.pid, info.active);
        println!("  endpoint: {}", info.endpoint_id);
        println!("  volume: {:?}", s.volume(&info.instance_id));
        println!("  muted: {:?}", s.muted(&info.instance_id));
        println!(
            "  session lookup: {:?}",
            s.session(&info.instance_id).map(|o| o.is_some())
        );
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("sessions_probe is Windows-only");
}
