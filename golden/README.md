# Golden frames

`frames.json` pins down the wire format in `schema/rpc.fbs` as bytes, for every
implementation of the core to test its decoder against. It's written by
`core/tests/golden.rs`; don't edit it by hand.

It's an array of frames. Every frame has:

- `name`
- `wire`: the frame as sent, in hex: COBS-encoded, with the trailing `00`.

A valid frame also has:

- `header`: the `Rpc.Header`, as flatc prints it
  (`flatc --json --strict-json --defaults-json --raw-binary --size-prefixed`).
- `payload`: for the framework's payloads (`Hello`, `Credit`), the `Rpc.Hello`
  or `Rpc.Credits` as flatc prints it; otherwise the payload's bytes in hex
  (`""` when empty).

The expected values come from flatc, the reference flatbuffers implementation,
so they don't depend on the Rust core under test.

An invalid frame has `error` instead, the error decoding must fail with:

| `error`     | Meaning                                                           |
|-------------|-------------------------------------------------------------------|
| `Cobs`      | invalid COBS                                                      |
| `TooShort`  | no room for the CRC and the header's size prefix                  |
| `Crc`       | CRC mismatch                                                      |
| `BadHeader` | the CRC passes, but the header overruns the frame or fails to verify |
| `BadLayout` | non-zero padding before the payload                               |

## Using it

A decoder must decode every valid frame's `wire` to exactly its `header` and
`payload`, and reject every invalid one with its `error`.

Encoders need not reproduce the `wire` bytes: flatbuffers builders may order
fields, share vtables and pad differently. Check an encoder by decoding its
frames instead.

## Changing it

The Rust tests fail when the encoder's output no longer matches `frames.json`.
If the wire format change is meant, regenerate (this needs flatc on the PATH):

```sh
UPDATE_GOLDEN=1 cargo test -p rpc-core --test golden
```

and update the other implementations to match.
