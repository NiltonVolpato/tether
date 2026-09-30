//! Golden frames: the wire format pinned down as bytes, for other
//! implementations of the core (the C++ one) to test their decoders against.
//!
//! The test fails when the encoder's output changes. If the change is meant,
//! regenerate with `UPDATE_GOLDEN=1 cargo test -p rpc-core --test golden` and
//! commit `golden/frames.txt` along with it.

use std::fmt::Write;

use rpc_core::frame::{self, FrameError, Header};
use rpc_core::proto::{self, Credits, CreditsArgs, Grant, Hello, HelloArgs, Kind, Status};

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/frames.txt");

const PREAMBLE: &str = "\
# Golden frames for the wire format in schema/rpc.fbs.
# Written by core/tests/golden.rs; do not edit.
#
# One block per frame, blocks separated by a blank line, one `key value...`
# per line. Integers are decimal, bytes are lowercase hex (`-` when empty).
#
# `wire` is the frame as sent: COBS-encoded, with the trailing 0x00.
#
# A frame with `kind` must decode to exactly these header fields and payload
# bytes. Framework payloads are given as fields too: `hello` is a Hello's
# boot_id and peer_boot_id, and each `grant` is one of a Credits' grants
# (call_id, credit), in order.
#
# A frame with `error` must fail to decode with that error:
#   Cobs       invalid COBS
#   TooShort   no room for the CRC and the header's size prefix
#   Crc        CRC mismatch
#   BadHeader  CRC passes, but the header overruns the frame or fails to verify
#   BadLayout  non-zero padding before the payload
#
# Encoders need not reproduce these bytes: flatbuffers builders may order
# fields, share vtables and pad differently. An encoder's frames must decode to
# the same fields instead.
";

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
    if bytes.is_empty() {
        return "-".into();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn render(cases: &[Case]) -> String {
    let mut out = String::from(PREAMBLE);
    for case in cases {
        writeln!(out, "\nname {}", case.name).unwrap();
        match &case.expect {
            Expect::Frame { header: h, payload, hello, grants } => {
                writeln!(out, "kind {}", h.kind.variant_name().unwrap()).unwrap();
                writeln!(out, "seq {}", h.seq).unwrap();
                writeln!(out, "call_id {}", h.call_id).unwrap();
                writeln!(out, "service {}", h.service).unwrap();
                writeln!(out, "method {}", h.method).unwrap();
                writeln!(out, "credit {}", h.credit).unwrap();
                writeln!(out, "status {}", h.status.variant_name().unwrap()).unwrap();
                writeln!(out, "payload {}", hex(payload)).unwrap();
                if let Some((boot_id, peer_boot_id)) = hello {
                    writeln!(out, "hello {boot_id} {peer_boot_id}").unwrap();
                }
                for (call_id, credit) in grants {
                    writeln!(out, "grant {call_id} {credit}").unwrap();
                }
            }
            Expect::Error(e) => writeln!(out, "error {e:?}").unwrap(),
        }
        writeln!(out, "wire {}", hex(&case.wire)).unwrap();
    }
    out
}

#[test]
fn golden_frames_are_unchanged() {
    let rendered = render(&cases());
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(std::path::Path::new(PATH).parent().unwrap()).unwrap();
        std::fs::write(PATH, &rendered).unwrap();
        return;
    }
    let stored = std::fs::read_to_string(PATH).unwrap_or_default();
    assert!(
        stored == rendered,
        "golden/frames.txt differs from the encoder's output. If the wire format \
         change is meant, regenerate with UPDATE_GOLDEN=1 and update the C++ core too."
    );
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
    for kind in proto::Kind::ENUM_VALUES {
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
