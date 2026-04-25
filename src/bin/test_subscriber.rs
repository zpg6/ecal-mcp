//! Tiny eCAL string subscriber used by the realnet e2e test suite.
//!
//! Subscribes to the configured topic and stays alive until terminated.
//! Used to give the realnet topology a *long-lived, remote* subscriber
//! so `ecal_list_subscribers` and `ecal_diagnose_topic` have a stable
//! sub-direction registration to surface across the bridge. The
//! MCP-side ad-hoc subscribers (inside `ecal_subscribe` /
//! `ecal_diagnose_topic`) are short-lived measurement windows and may
//! not be present at the moment a snapshot is taken, so the realnet
//! suite leans on this binary to deterministically assert sub-direction
//! cross-bridge multicast registration.
//!
//! Usage:
//!   ecal-test-subscriber [--topic NAME]

use std::env;
use std::thread;
use std::time::Duration;

use rustecal::{Ecal, EcalComponents, TypedSubscriber};
use rustecal_types_string::StringMessage;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut topic = "ecal_mcp_realnet_pub".to_string();

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--topic" if i + 1 < args.len() => {
                topic = args[i + 1].clone();
                i += 2;
            }
            other => panic!("unknown argument: {other}"),
        }
    }

    Ecal::initialize(Some("ecal_test_subscriber"), EcalComponents::DEFAULT, None)?;

    let mut subscriber = TypedSubscriber::<StringMessage>::new(&topic)?;
    // The realnet harness only inspects this process's *registration*
    // (via ecal_list_subscribers / ecal_diagnose_topic from MCP) — we
    // don't need to do anything with the data, but a callback keeps
    // the receive path live and proves data actually drains.
    subscriber.set_callback(|_received| {});

    eprintln!("test subscriber: topic={topic}");

    while Ecal::ok() {
        thread::sleep(Duration::from_millis(200));
    }

    Ecal::finalize();
    Ok(())
}
