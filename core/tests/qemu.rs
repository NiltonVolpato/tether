//! Integration with the C++ core running on an ESP32: the test app in
//! cpp/test_app, with its UART1 at the serial device `TETHER_UART`, either
//! QEMU's pty or a USB-UART adapter wired to a board (`TETHER_UART_BAUD` sets
//! the rate; 0 for a pty). `make -C cpp qemu-test` starts QEMU and runs these;
//! they're ignored otherwise.
//!
//! For now the device echoes frames, so these check that the two cores agree
//! on the wire format when the C++ one is built for, and runs on, the target.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use tether_core::frame::{self, Deframer, Frame, Header};
use tether_core::wire::{Credits, CreditsArgs, Grant, Hello, HelloArgs, Kind, Status};

/// The device serves one connection at a time.
static DEVICE: Mutex<()> = Mutex::new(());

struct Device {
    port: Box<dyn serialport::SerialPort>,
    deframer: Deframer,
    received: VecDeque<Frame>,
}

impl Device {
    /// Connects (retrying while QEMU boots) and syncs: pings until one is
    /// echoed, since the device drops what arrives before its UART is set up.
    /// It answers in order, so nothing sent before that echo can follow it.
    fn connect() -> Self {
        let path = std::env::var("TETHER_UART").expect("TETHER_UART: the device's serial port");
        // The test app's rate. 0 is for a pty, which has none (and on macOS
        // fails when given one).
        let baud = std::env::var("TETHER_UART_BAUD").map_or(921_600, |b| b.parse().unwrap());
        let deadline = Instant::now() + Duration::from_secs(60);
        // QEMU creates its pty as it starts.
        let port = loop {
            match serialport::new(&path, baud).timeout(Duration::from_secs(1)).open() {
                Ok(port) => break port,
                Err(e) if Instant::now() < deadline => {
                    eprintln!("waiting for {path}: {e}");
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => panic!("can't open {path}: {e}"),
            }
        };
        let mut device = Self { port, deframer: Deframer::new(4096), received: VecDeque::new() };
        let mut attempt = 0;
        while Instant::now() < deadline {
            attempt += 1;
            let sync = Header { call_id: 0x5EC0_0000 + attempt, ..Header::new(Kind::Ping) };
            device.send(&frame::encode(&sync, &[]));
            while let Some(frame) = device.try_recv() {
                if frame.header == sync {
                    device.port.set_timeout(Duration::from_secs(10)).unwrap();
                    return device;
                }
            }
        }
        panic!("the device at {path} doesn't answer");
    }

    fn send(&mut self, bytes: &[u8]) {
        self.port.write_all(bytes).unwrap();
    }

    fn recv(&mut self) -> Frame {
        self.try_recv().expect("no frame from the device")
    }

    /// The next frame, or `None` if none arrives within the read timeout.
    fn try_recv(&mut self) -> Option<Frame> {
        let mut buf = [0; 512];
        loop {
            if let Some(frame) = self.received.pop_front() {
                return Some(frame);
            }
            let n = match self.port.read(&mut buf) {
                Ok(0) => panic!("the device closed the connection"),
                Ok(n) => n,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return None;
                }
                Err(e) => panic!("{e}"),
            };
            for &b in &buf[..n] {
                if let Some(result) = self.deframer.push(b) {
                    self.received.push_back(result.expect("the device sent a bad frame"));
                }
            }
        }
    }

    fn echo(&mut self, header: Header, payload: &[u8]) -> Frame {
        self.send(&frame::encode(&header, payload));
        self.recv()
    }
}

fn hello(boot_id: u32, peer_boot_id: u32) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let hello = Hello::create(&mut fbb, &HelloArgs { boot_id, peer_boot_id });
    fbb.finish(hello, None);
    fbb.finished_data().to_vec()
}

fn credits(n: u32) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let grants: Vec<_> = (0..n).map(|i| Grant::new(i * 7919, i as u16)).collect();
    let grants = fbb.create_vector(&grants);
    let credits = Credits::create(&mut fbb, &CreditsArgs { grants: Some(grants) });
    fbb.finish(credits, None);
    fbb.finished_data().to_vec()
}

fn golden_frames() -> Vec<serde_json::Value> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/frames.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
#[ignore = "needs the test app in QEMU; run `make -C cpp qemu-test`"]
fn echoes_the_golden_frames() {
    let _device = DEVICE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut device = Device::connect();
    for golden in golden_frames().iter().filter(|g| g.get("error").is_none()) {
        let wire = unhex(golden["wire"].as_str().unwrap());
        let sent = frame::decode(&wire[..wire.len() - 1]).unwrap();
        device.send(&wire);
        assert_eq!(device.recv(), sent, "{}", golden["name"]);
    }
}

#[test]
#[ignore = "needs the test app in QEMU; run `make -C cpp qemu-test`"]
fn echoes_every_kind_with_every_padding() {
    let _device = DEVICE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut device = Device::connect();
    let mut sent = 0;
    for &kind in Kind::ENUM_VALUES {
        let payloads: Vec<Vec<u8>> = match kind {
            Kind::Hello => vec![hello(1, 0), hello(u32::MAX, 0xC11E_0001)],
            Kind::Credit => (0..=9).map(credits).collect(),
            // Zeros for COBS, every length modulo 8 for the padding, and runs
            // past one COBS block.
            _ => [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 15, 16, 17, 253, 254, 255, 256, 511, 900]
                .iter()
                .map(|&len| (0..len).map(|i| (i * 31 % 251) as u8).collect())
                .collect(),
        };
        for (i, payload) in payloads.iter().enumerate() {
            let header = Header {
                kind,
                seq: (sent * 257) as u16,
                call_id: 0xA000_0000 | sent,
                service: i as u8,
                method: (i * 3) as u8,
                credit: (i * 1000) as u16,
                status: Status(i as i8 % 17),
            };
            let echo = device.echo(header, payload);
            assert_eq!((echo.header, &echo.payload), (header, payload), "{kind:?}, {i}");
            sent += 1;
        }
    }
}

#[test]
#[ignore = "needs the test app in QEMU; run `make -C cpp qemu-test`"]
fn drops_bad_frames_and_resyncs() {
    let _device = DEVICE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut device = Device::connect();
    let bad: Vec<_> = (golden_frames().iter())
        .filter(|g| g.get("error").is_some())
        .map(|g| unhex(g["wire"].as_str().unwrap()))
        .collect();
    assert!(!bad.is_empty());
    let cancel = |call_id| Header { call_id, ..Header::new(Kind::Cancel) };
    for (i, wire) in bad.iter().enumerate() {
        // A bad frame, then noise up to a delimiter: neither costs the next frame.
        device.send(wire);
        device.send(&[0x55, 0xAA, 0x13, 0x00]);
        assert_eq!(device.echo(cancel(i as u32), &[]).header, cancel(i as u32), "after {i}");
    }
    // Noise without a delimiter becomes the start of the next frame, which is
    // lost (the link retransmits it); the one after that is fine.
    device.send(&[0x55, 0xAA, 0x13]);
    device.send(&frame::encode(&cancel(100), &[]));
    assert_eq!(device.echo(cancel(101), &[]).header, cancel(101));
}

#[test]
#[ignore = "needs the test app in QEMU; run `make -C cpp qemu-test`"]
fn verifies_framework_payloads_on_the_device() {
    let _device = DEVICE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut device = Device::connect();
    let header = Header::new(Kind::Hello);
    assert_eq!(device.echo(header, &hello(7, 9)).header.status, Status::OK);
    let garbage = [0xFF; 24];
    assert_eq!(device.echo(header, &garbage).header.status, Status::DATA_LOSS);
    let header = Header::new(Kind::Credit);
    assert_eq!(device.echo(header, &credits(3)).header.status, Status::OK);
    assert_eq!(device.echo(header, &garbage).header.status, Status::DATA_LOSS);
}
