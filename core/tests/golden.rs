//! Golden frames: the wire format pinned down as bytes, for other
//! implementations of the core (the C++ one) to test their decoders against.
//!
//! `golden/frames.json` holds each frame's wire bytes and, for the valid ones,
//! flatc's JSON decoding of its header and framework payload: expected values
//! that come from the reference flatbuffers implementation, not from this crate.
//! The format is described in `golden/README.md`.
//!
//! The tests fail when the encoder's output changes. If the change is meant,
//! regenerate (this needs flatc) with
//! `UPDATE_GOLDEN=1 cargo test -p tether-core --test golden` and commit the file.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use tether_core::frame::{self, FrameError, Header};
use tether_core::wire::{self, Credits, CreditsArgs, Grant, Hello, HelloArgs, Kind, Status};
use serde_json::{Value, json};

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/frames.json");
const SCHEMA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../schema/wire.fbs");

enum Expect {
    Frame {
        header: Header,
        payload: Vec<u8>,
        hello: Option<(u32, u32)>,
        grants: Vec<(u32, u16)>,
    },
    Error(FrameError),
}

struct Case {
    name: &'static str,
    wire: Vec<u8>,
    expect: Expect,
}

fn frame(name: &'static str, header: Header, payload: Vec<u8>) -> Case {
    let wire = frame::encode(&header, &payload);
    Case {
        name,
        wire,
        expect: Expect::Frame { header, payload, hello: None, grants: Vec::new() },
    }
}

fn hello(name: &'static str, boot_id: u32, peer_boot_id: u32) -> Case {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let hello = Hello::create(&mut fbb, &HelloArgs { boot_id, peer_boot_id });
    fbb.finish(hello, None);
    let mut case = frame(name, Header::new(Kind::Hello), fbb.finished_data().to_vec());
    if let Expect::Frame { hello, .. } = &mut case.expect {
        *hello = Some((boot_id, peer_boot_id));
    }
    case
}

fn credit(name: &'static str, seq: u16, grants: &[(u32, u16)]) -> Case {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let list: Vec<_> =
        grants.iter().map(|&(call_id, credit)| Grant::new(call_id, credit)).collect();
    let list = fbb.create_vector(&list);
    let credits = Credits::create(&mut fbb, &CreditsArgs { grants: Some(list) });
    fbb.finish(credits, None);
    let header = Header { seq, ..Header::new(Kind::Credit) };
    let mut case = frame(name, header, fbb.finished_data().to_vec());
    if let Expect::Frame { grants: g, .. } = &mut case.expect {
        *g = grants.to_vec();
    }
    case
}

fn error(name: &'static str, body: Vec<u8>, error: FrameError) -> Case {
    let mut wire = cobs::encode_vec(&body);
    wire.push(0);
    Case { name, wire, expect: Expect::Error(error) }
}

/// A frame body (CRC onwards, before COBS) with its CRC recomputed.
fn with_crc(mut body: Vec<u8>) -> Vec<u8> {
    let crc = crc32fast::hash(&body[4..]);
    body[..4].copy_from_slice(&crc.to_le_bytes());
    body
}

fn body_of(header: Header, payload: &[u8]) -> Vec<u8> {
    let wire = frame::encode(&header, payload);
    cobs::decode_vec(&wire[..wire.len() - 1]).unwrap()
}

fn bytes(len: usize, f: impl Fn(usize) -> u8) -> Vec<u8> {
    (0..len).map(f).collect()
}

fn cases() -> Vec<Case> {
    let item = |seq, call_id| Header { seq, call_id, ..Header::new(Kind::Item) };
    vec![
        // Link
        hello("hello", 0xC11E_0001, 0),
        hello("hello_reply", 0x5E4E_0001, 0xC11E_0001),
        frame("ack", Header { seq: 1234, ..Header::new(Kind::Ack) }, vec![]),
        frame("ping", Header { seq: 7, ..Header::new(Kind::Ping) }, vec![]),
        // Client -> server
        frame(
            "request",
            Header {
                seq: 1,
                call_id: 57,
                service: 4,
                method: 1,
                credit: 1,
                ..Header::new(Kind::Request)
            },
            bytes(32, |i| i as u8 + 1),
        ),
        frame(
            "open",
            Header {
                seq: 2,
                call_id: 58,
                service: 1,
                method: 0,
                credit: 2,
                ..Header::new(Kind::Open)
            },
            bytes(8, |i| 0xF0 | i as u8),
        ),
        credit("credit", 3, &[(58, 1)]),
        credit("credit_many", 4, &[(57, 1), (58, 2), (u32::MAX, u16::MAX)]),
        frame("cancel", Header { seq: 5, call_id: 58, ..Header::new(Kind::Cancel) }, vec![]),
        // Server -> client
        frame(
            "response_ok",
            Header { seq: 1, call_id: 57, ..Header::new(Kind::Response) },
            bytes(9, |i| 9 - i as u8),
        ),
        frame(
            "response_error",
            Header {
                seq: 1,
                call_id: 57,
                status: Status::UNIMPLEMENTED,
                ..Header::new(Kind::Response)
            },
            vec![],
        ),
        // Zeros every 256 bytes, for COBS.
        frame("item_512", item(2, 58), bytes(512, |i| i as u8)),
        // A run longer than one COBS block (254 bytes).
        frame("item_no_zeros", item(3, 58), vec![0xA5; 300]),
        frame("item_1", item(4, 58), vec![0]),
        frame("end_ok", Header { seq: 5, call_id: 58, ..Header::new(Kind::End) }, vec![]),
        frame(
            "end_error",
            Header {
                seq: 6,
                call_id: 58,
                status: Status::RESOURCE_EXHAUSTED,
                ..Header::new(Kind::End)
            },
            vec![],
        ),
        // Every header field at its maximum.
        frame(
            "max_fields",
            Header {
                kind: Kind::Item,
                seq: u16::MAX,
                call_id: u32::MAX,
                service: u8::MAX,
                method: u8::MAX,
                credit: u16::MAX,
                status: Status::DATA_LOSS,
            },
            vec![0xFF; 7],
        ),
        // Errors
        // The code byte promises 4 more bytes before the delimiter; there's 1.
        Case {
            name: "error_cobs",
            wire: vec![0x05, 0x01, 0x00],
            expect: Expect::Error(FrameError::Cobs),
        },
        error("error_too_short", vec![1, 2, 3, 4, 5, 6, 7], FrameError::TooShort),
        error(
            "error_crc",
            {
                let mut body = body_of(Header { seq: 1234, ..Header::new(Kind::Ack) }, &[]);
                *body.last_mut().unwrap() ^= 0x01;
                body
            },
            FrameError::Crc,
        ),
        error(
            "error_header_overrun",
            with_crc(vec![0, 0, 0, 0, 0xFF, 0xFF, 0, 0, 1, 2, 3, 4]),
            FrameError::BadHeader,
        ),
        error(
            "error_bad_padding",
            {
                let header = Header { seq: 1, call_id: 57, ..Header::new(Kind::Response) };
                let mut body = body_of(header, &[1]);
                // The payload is the last byte; the padding ends just before it.
                let pad = body.len() - 2;
                assert_eq!(body[pad], 0, "expected padding before the payload");
                body[pad] = 0x01;
                with_crc(body)
            },
            FrameError::BadLayout,
        ),
    ]
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// The root type of a framework payload, which flatc can decode.
fn payload_type(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::Hello => Some("tether.wire.Hello"),
        Kind::Credit => Some("tether.wire.Credits"),
        _ => None,
    }
}

/// flatc's JSON for a flatbuffer of type `root`.
fn flatc_json(name: &str, bytes: &[u8], root: &str, size_prefixed: bool) -> Value {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("golden");
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join(format!("{name}.bin"));
    std::fs::write(&bin, bytes).unwrap();
    let mut flatc = Command::new("flatc");
    flatc.args(["--json", "--strict-json", "--defaults-json", "--raw-binary"]);
    if size_prefixed {
        flatc.arg("--size-prefixed");
    }
    flatc
        .args(["--root-type", root, "-o"])
        .arg(&dir)
        .arg(SCHEMA)
        .arg("--")
        .arg(&bin);
    let status = flatc.status().expect("regenerating the goldens needs flatc");
    assert!(status.success(), "flatc failed on {name}");
    let json = std::fs::read_to_string(dir.join(format!("{name}.json"))).unwrap();
    serde_json::from_str(&json).unwrap()
}

/// The frame as stored in `frames.json`.
fn golden(case: &Case) -> Value {
    let Expect::Frame { header, .. } = &case.expect else {
        let Expect::Error(e) = &case.expect else { unreachable!() };
        return json!({ "name": case.name, "wire": hex(&case.wire), "error": format!("{e:?}") });
    };
    // Split the body the way the decoder does, but by hand, so flatc sees the
    // exact bytes on the wire.
    let body = cobs::decode_vec(&case.wire[..case.wire.len() - 1]).unwrap();
    let header_end = 8 + u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
    let payload = match header_end == body.len() {
        true => &[][..],
        false => &body[header_end.next_multiple_of(8)..],
    };
    let name = case.name;
    json!({
        "name": name,
        "wire": hex(&case.wire),
        "header": flatc_json(&format!("{name}.header"), &body[4..header_end], "tether.wire.Header", true),
        "payload": match payload_type(header.kind) {
            Some(root) => flatc_json(&format!("{name}.payload"), payload, root, false),
            None => Value::String(hex(payload)),
        },
    })
}

/// The stored frames, regenerated first (once, as tests run in parallel) with
/// `UPDATE_GOLDEN`.
fn stored() -> &'static [Value] {
    static STORED: OnceLock<Vec<Value>> = OnceLock::new();
    STORED.get_or_init(|| {
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            let frames: Vec<_> = cases().iter().map(golden).collect();
            std::fs::create_dir_all(Path::new(PATH).parent().unwrap()).unwrap();
            std::fs::write(PATH, serde_json::to_string_pretty(&frames).unwrap() + "\n").unwrap();
        }
        let text = std::fs::read_to_string(PATH).expect("golden/frames.json");
        serde_json::from_str(&text).unwrap()
    })
}

/// A decoded header as flatc prints it.
fn header_json(h: &Header) -> Value {
    json!({
        "kind": h.kind.variant_name().unwrap(),
        "seq": h.seq,
        "call_id": h.call_id,
        "service": h.service,
        "method": h.method,
        "credit": h.credit,
        "status": h.status.variant_name().unwrap(),
    })
}

/// A decoded framework payload as flatc prints it.
fn payload_json(kind: Kind, payload: &[u8]) -> Value {
    match kind {
        Kind::Hello => {
            let h = flatbuffers::root::<Hello>(payload).unwrap();
            json!({ "boot_id": h.boot_id(), "peer_boot_id": h.peer_boot_id() })
        }
        Kind::Credit => {
            let c = flatbuffers::root::<Credits>(payload).unwrap();
            let grants: Vec<_> = (c.grants().unwrap().iter())
                .map(|g| json!({ "call_id": g.call_id(), "credit": g.credit() }))
                .collect();
            json!({ "grants": grants })
        }
        _ => Value::String(hex(payload)),
    }
}

#[test]
fn golden_frames_are_unchanged() {
    let stored = stored();
    let cases = cases();
    let names = |v: &[Value]| -> Vec<String> {
        v.iter().map(|f| f["name"].as_str().unwrap().to_owned()).collect()
    };
    let expected: Vec<_> = cases.iter().map(|c| c.name.to_owned()).collect();
    assert_eq!(names(stored), expected, "golden frames added, removed or reordered");
    for (golden, case) in stored.iter().zip(&cases) {
        assert!(
            golden["wire"] == hex(&case.wire),
            "{}: the encoder's output changed. If the wire format change is meant, \
             regenerate with UPDATE_GOLDEN=1 and update the C++ core too.",
            case.name
        );
    }
}

#[test]
fn golden_frames_decode_as_flatc_does() {
    for golden in stored() {
        let name = golden["name"].as_str().unwrap();
        let wire = unhex(golden["wire"].as_str().unwrap());
        let decoded = frame::decode(&wire[..wire.len() - 1]);
        if let Some(error) = golden.get("error") {
            let got = decoded.map(|f| f.header).map_err(|e| format!("{e:?}"));
            assert_eq!(got, Err(error.as_str().unwrap().to_owned()), "{name}");
            continue;
        }
        let f = decoded.unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(header_json(&f.header), golden["header"], "{name}: header");
        assert_eq!(payload_json(f.header.kind, &f.payload), golden["payload"], "{name}: payload");
    }
}

#[test]
fn golden_frames_decode() {
    for case in cases() {
        let name = case.name;
        let (&delimiter, cobs_frame) = case.wire.split_last().unwrap();
        assert_eq!(delimiter, 0, "{name}");
        assert!(!cobs_frame.contains(&0), "{name}: stray delimiter");
        let decoded = frame::decode(cobs_frame);
        match case.expect {
            Expect::Error(e) => assert_eq!(decoded, Err(e), "{name}"),
            Expect::Frame { header, payload, hello, grants } => {
                let f = decoded.unwrap_or_else(|e| panic!("{name}: {e:?}"));
                assert_eq!((f.header, &f.payload), (header, &payload), "{name}");
                if let Some(expected) = hello {
                    let h = flatbuffers::root::<Hello>(&f.payload).unwrap();
                    assert_eq!((h.boot_id(), h.peer_boot_id()), expected, "{name}");
                }
                if f.header.kind == Kind::Credit {
                    let c = flatbuffers::root::<Credits>(&f.payload).unwrap();
                    let got: Vec<_> =
                        c.grants().unwrap().iter().map(|g| (g.call_id(), g.credit())).collect();
                    assert_eq!(got, grants, "{name}");
                }
            }
        }
    }
}

#[test]
fn golden_frames_cover_every_kind_and_error() {
    let cases = cases();
    for kind in wire::Kind::ENUM_VALUES {
        assert!(
            cases
                .iter()
                .any(|c| matches!(&c.expect, Expect::Frame { header, .. } if header.kind == *kind)),
            "no golden frame of kind {kind:?}"
        );
    }
    for error in [
        FrameError::Cobs,
        FrameError::TooShort,
        FrameError::Crc,
        FrameError::BadHeader,
        FrameError::BadLayout,
    ] {
        assert!(
            cases.iter().any(|c| matches!(c.expect, Expect::Error(e) if e == error)),
            "no golden frame for {error:?}"
        );
    }
}
