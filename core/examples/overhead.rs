//! Prints the wire size of typical frames.

use tether_core::frame::{self, Header};
use tether_core::wire::{Credits, CreditsArgs, Grant, Kind, Status};

fn credits(n: u32) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let grants: Vec<_> = (0..n).map(|i| Grant::new(57 + i, 1)).collect();
    let grants = fbb.create_vector(&grants);
    let credits = Credits::create(&mut fbb, &CreditsArgs { grants: Some(grants) });
    fbb.finish(credits, None);
    fbb.finished_data().to_vec()
}

fn main() {
    let cases = [
        ("Ack", Header { seq: 1234, ..Header::new(Kind::Ack) }, vec![]),
        ("Credit, 1 grant", Header { seq: 1234, ..Header::new(Kind::Credit) }, credits(1)),
        (
            "Credit, 3 grants",
            Header { seq: 1234, ..Header::new(Kind::Credit) },
            credits(3),
        ),
        (
            "Request, 32 B",
            Header {
                seq: 1234,
                call_id: 57,
                service: 4,
                method: 1,
                credit: 1,
                ..Header::new(Kind::Request)
            },
            vec![0xA5; 32],
        ),
        (
            "Item, 512 B",
            Header { seq: 1234, call_id: 57, ..Header::new(Kind::Item) },
            vec![0xA5; 512],
        ),
        (
            "End",
            Header { seq: 1234, call_id: 57, status: Status::OK, ..Header::new(Kind::End) },
            vec![],
        ),
    ];
    for (name, header, payload) in cases {
        let wire = frame::encode(&header, &payload).len();
        println!("{name:>16}: {wire:4} bytes on the wire ({} of overhead)", wire - payload.len());
    }
}
