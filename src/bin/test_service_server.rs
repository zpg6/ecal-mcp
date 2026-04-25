//! Tiny eCAL service server used by the e2e test suite.
//!
//! Registers two methods on the configured service:
//!   - `echo`    : returns the request payload unchanged.
//!   - `reverse` : returns the request payload byte-reversed.

use std::env;
use std::thread;
use std::time::Duration;

use rustecal::service::types::MethodInfo;
use rustecal::{Ecal, EcalComponents, ServiceServer};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut service_name = "ecal_mcp_test_service".to_string();

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--service" if i + 1 < args.len() => {
                service_name = args[i + 1].clone();
                i += 2;
            }
            other => panic!("unknown argument: {other}"),
        }
    }

    Ecal::initialize(
        Some("ecal_test_service_server"),
        EcalComponents::DEFAULT,
        None,
    )?;

    let mut server = ServiceServer::new(&service_name)?;
    server.add_method(
        "echo",
        Box::new(|_info: MethodInfo, req: &[u8]| -> Vec<u8> { req.to_vec() }),
    )?;
    server.add_method(
        "reverse",
        Box::new(|_info: MethodInfo, req: &[u8]| -> Vec<u8> {
            let mut v = req.to_vec();
            v.reverse();
            v
        }),
    )?;

    eprintln!("test service server: name={service_name} methods=[echo, reverse]");

    while Ecal::ok() {
        thread::sleep(Duration::from_millis(200));
    }

    Ecal::finalize();
    Ok(())
}
