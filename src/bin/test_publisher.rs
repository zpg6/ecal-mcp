//! Tiny eCAL string publisher used by the e2e test suite.
//!
//! Publishes a sequence of `"<prefix> <counter>"` strings on the configured
//! topic at a fixed rate until terminated.
//!
//! Usage:
//!   ecal-test-publisher [--topic NAME] [--prefix TEXT] [--interval-ms N]

use std::env;
use std::thread;
use std::time::Duration;

use rustecal::pubsub::publisher::Timestamp;
use rustecal::{Ecal, EcalComponents, TypedPublisher};
use rustecal_types_string::StringMessage;

fn parse_args() -> (String, String, u64) {
    let mut topic = "ecal_mcp_e2e".to_string();
    let mut prefix = "hello-from-e2e".to_string();
    let mut interval_ms: u64 = 100;

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--topic" if i + 1 < args.len() => {
                topic = args[i + 1].clone();
                i += 2;
            }
            "--prefix" if i + 1 < args.len() => {
                prefix = args[i + 1].clone();
                i += 2;
            }
            "--interval-ms" if i + 1 < args.len() => {
                interval_ms = args[i + 1].parse().expect("invalid --interval-ms");
                i += 2;
            }
            other => panic!("unknown argument: {other}"),
        }
    }
    (topic, prefix, interval_ms)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (topic, prefix, interval_ms) = parse_args();

    Ecal::initialize(Some("ecal_test_publisher"), EcalComponents::DEFAULT, None)?;
    let publisher = TypedPublisher::<StringMessage>::new(&topic)?;
    eprintln!("test publisher: topic={topic} interval_ms={interval_ms}");

    let mut counter: u64 = 0;
    while Ecal::ok() {
        let msg = StringMessage {
            data: format!("{prefix} {counter}").into(),
        };
        publisher.send(&msg, Timestamp::Auto);
        counter = counter.wrapping_add(1);
        thread::sleep(Duration::from_millis(interval_ms));
    }

    Ecal::finalize();
    Ok(())
}
