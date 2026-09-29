//! Prints the wire size of typical frames.

use rpc_experiment::frame::{self, Header};
use rpc_experiment::proto::{Kind, Status};

fn main() {
    let cases = [
        ("Ack", Header { seq: 1234, ..Header::new(Kind::Ack) }, 0),
        (
            "Credit",
            Header { seq: 1234, call_id: 57, credit: 1, ..Header::new(Kind::Credit) },
            0,
        ),
        (
            "Request, 32 B",
            Header {
                seq: 1234,
                call_id: 57,
                method: 0x1062_9345,
                credit: 1,
                ..Header::new(Kind::Request)
            },
            32,
        ),
        ("Item, 512 B", Header { seq: 1234, call_id: 57, ..Header::new(Kind::Item) }, 512),
        (
            "End",
            Header { seq: 1234, call_id: 57, status: Status::OK, ..Header::new(Kind::End) },
            0,
        ),
    ];
    for (name, header, len) in cases {
        let payload = vec![0xA5; len];
        let wire = frame::encode(&header, &payload).len();
        println!("{name:>14}: {wire:4} bytes on the wire ({} of overhead)", wire - len);
    }
}
