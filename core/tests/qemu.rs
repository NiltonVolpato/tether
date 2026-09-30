//! Integration with the C++ core running on an ESP32: the test app in
//! cpp/test_app, with its UART1 at the serial device `TETHER_UART`, either
//! QEMU's pty or a USB-UART adapter wired to a board (`TETHER_UART_BAUD` sets
//! the rate; 0 for a pty). `make -C cpp qemu-test` starts QEMU and runs these;
//! they're ignored otherwise.
//!
//! The device runs the C++ link and echoes every frame it delivers, so the Rust
//! link here talks to the C++ one: handshake, retransmits, and both cores'
//! framing, with the C++ one built for and running on the target.

use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant, SystemTime};

use tether_core::frame::{Frame, Header};
use tether_core::link::{Link, LinkConfig, LinkState};
use tether_core::wire::{Credits, CreditsArgs, Grant, Kind, Status};

/// The device serves one test at a time.
static DEVICE: Mutex<()> = Mutex::new(());

/// The Rust end of a link to the device. A thread reads the port all the
/// time: QEMU blocks on writing to a full pty, so a reader that also writes
/// can deadlock with it.
struct Device {
    port: Box<dyn serialport::SerialPort>,
    received: mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    link: Link,
    start: Instant,
}

impl Device {
    /// Opens the port (retrying while QEMU starts) and links with a new boot
    /// id. The device sees a reboot, starts a new link of its own, and links
    /// with this one.
    fn connect() -> Self {
        let path = std::env::var("TETHER_UART").expect("TETHER_UART: the device's serial port");
        // The test app's rate. 0 is for a pty, which has none (and on macOS
        // fails when given one).
        let baud = std::env::var("TETHER_UART_BAUD").map_or(921_600, |b| b.parse().unwrap());
        let deadline = Instant::now() + Duration::from_secs(60);
        let port = loop {
            match serialport::new(&path, baud).timeout(Duration::from_millis(10)).open() {
                Ok(port) => break port,
                Err(e) if Instant::now() < deadline => {
                    eprintln!("waiting for {path}: {e}");
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => panic!("can't open {path}: {e}"),
            }
        };
        let (tx, received) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let mut port = port.try_clone().unwrap();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut buf = [0; 512];
                while !stop.load(Ordering::Relaxed) {
                    match port.read(&mut buf) {
                        Ok(n) => tx.send(buf[..n].to_vec()).unwrap(),
                        Err(e) if matches!(e.kind(), ErrorKind::TimedOut) => {}
                        Err(e) => panic!("{e}"),
                    }
                }
            })
        };
        let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        let link = Link::new(nanos as u32 | 1, LinkConfig::default());
        let mut device =
            Self { port, received, stop, reader: Some(reader), link, start: Instant::now() };
        device.pump_until(Duration::from_secs(60), |d| {
            matches!(d.link.state(), LinkState::Linked { .. })
        });
        device
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Runs the link until `done` holds.
    fn pump_until(&mut self, timeout: Duration, mut done: impl FnMut(&mut Self) -> bool) {
        let deadline = Instant::now() + timeout;
        while !done(self) {
            let (state, stats) = (self.link.state(), self.link.stats());
            assert!(Instant::now() < deadline, "timed out; link {state:?}, {stats:?}");
            assert!(!state.is_terminal(), "link {state:?}, {stats:?}");
            while let Some(wire) = self.link.poll_transmit(self.now()) {
                self.write(&wire);
            }
            // Everything that arrived, before sending again: writes can block
            // while QEMU drains the pty, and acks mustn't wait behind them.
            match self.received.recv_timeout(Duration::from_millis(5)) {
                Ok(bytes) => self.link.receive(&bytes),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(e) => panic!("the reader stopped: {e}"),
            }
            while let Ok(bytes) = self.received.try_recv() {
                self.link.receive(&bytes);
            }
        }
    }

    /// Sends a frame and waits for the device to echo it.
    fn echo(&mut self, header: Header, payload: &[u8]) -> Frame {
        self.link.send(header, payload.to_vec());
        let mut echo = None;
        self.pump_until(Duration::from_secs(10), |d| {
            echo = echo.take().or_else(|| d.link.poll_receive());
            echo.is_some()
        });
        echo.unwrap()
    }

    /// Writes bytes straight to the port, around the link.
    fn inject(&mut self, bytes: &[u8]) {
        self.write(bytes);
    }

    /// Writes all of `bytes`, waiting while the device doesn't read (it
    /// doesn't while it boots).
    fn write(&mut self, mut bytes: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !bytes.is_empty() {
            assert!(Instant::now() < deadline, "the device isn't reading");
            match self.port.write(bytes) {
                Ok(n) => bytes = &bytes[n..],
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut) => {}
                Err(e) => panic!("{e}"),
            }
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // Stop reading, so the next test's connection gets every byte.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
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

/// The echo, with the seq the device's link gave it replaced by ours.
fn as_sent(echo: Frame, sent: &Header) -> (Header, Vec<u8>) {
    (Header { seq: sent.seq, ..echo.header }, echo.payload)
}

#[test]
#[ignore = "needs the test app in QEMU; run `make -C cpp qemu-test`"]
fn links_with_the_device() {
    let _device = DEVICE.lock().unwrap_or_else(PoisonError::into_inner);
    let device = Device::connect();
    let LinkState::Linked { peer_boot_id } = device.link.state() else {
        unreachable!()
    };
    assert_ne!(peer_boot_id, 0);
}

#[test]
#[ignore = "needs the test app in QEMU; run `make -C cpp qemu-test`"]
fn echoes_every_kind_with_every_padding() {
    let _device = DEVICE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut device = Device::connect();
    let mut sent = 0u32;
    // The kinds the link delivers; it handles Hello, Ack and Ping itself.
    for kind in [
        Kind::Request,
        Kind::Open,
        Kind::Response,
        Kind::Item,
        Kind::End,
        Kind::Credit,
        Kind::Cancel,
    ] {
        let payloads: Vec<Vec<u8>> = match kind {
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
                call_id: 0xA000_0000 | sent,
                service: i as u8,
                method: (i * 3) as u8,
                credit: (i * 1000) as u16,
                status: Status(i as i8 % 17),
                ..Header::new(kind)
            };
            let echo = device.echo(header, payload);
            assert_eq!(as_sent(echo, &header), (header, payload.clone()), "{kind:?}, {i}");
            sent += 1;
        }
    }
}

#[test]
#[ignore = "needs the test app in QEMU; run `make -C cpp qemu-test`"]
fn recovers_from_bad_frames_and_line_noise() {
    let _device = DEVICE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut device = Device::connect();
    let bad: Vec<_> = (golden_frames().iter())
        .filter(|g| g.get("error").is_some())
        .map(|g| unhex(g["wire"].as_str().unwrap()))
        .collect();
    assert!(!bad.is_empty());
    for (i, wire) in bad.iter().enumerate() {
        // A bad frame, then noise without a delimiter: the next frame, ours or
        // an ack, is lost, and the link retransmits.
        device.inject(wire);
        device.inject(&[0x55, 0xAA, 0x13]);
        let header = Header { call_id: i as u32, ..Header::new(Kind::Cancel) };
        let echo = device.echo(header, &[1, 2, 3]);
        assert_eq!(as_sent(echo, &header), (header, vec![1, 2, 3]), "after {i}");
    }
    // Some frame was lost, in one direction or the other.
    let stats = device.link.stats();
    assert!(stats.retransmits + stats.duplicates > 0, "the noise cost no frame: {stats:?}");
}

#[test]
#[ignore = "needs the test app in QEMU; run `make -C cpp qemu-test`"]
fn verifies_credits_on_the_device() {
    let _device = DEVICE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut device = Device::connect();
    let header = Header::new(Kind::Credit);
    assert_eq!(device.echo(header, &credits(3)).header.status, Status::OK);
    assert_eq!(device.echo(header, &[0xFF; 24]).header.status, Status::DATA_LOSS);
}
