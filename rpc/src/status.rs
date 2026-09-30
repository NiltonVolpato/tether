/// Why a call failed. The codes are gRPC's; success is `Ok(..)`, so there is
/// no `Ok` status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Status {
    Cancelled = 1,
    Unknown = 2,
    InvalidArgument = 3,
    DeadlineExceeded = 4,
    NotFound = 5,
    AlreadyExists = 6,
    PermissionDenied = 7,
    ResourceExhausted = 8,
    FailedPrecondition = 9,
    Aborted = 10,
    OutOfRange = 11,
    Unimplemented = 12,
    Internal = 13,
    Unavailable = 14,
    DataLoss = 15,
    Unauthenticated = 16,
}

impl Status {
    /// The gRPC code.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// `Ok(())` for code 0; codes this version doesn't know are `Unknown`.
    pub const fn from_code(code: u8) -> Result<(), Status> {
        use Status::*;
        Err(match code {
            0 => return Ok(()),
            1 => Cancelled,
            3 => InvalidArgument,
            4 => DeadlineExceeded,
            5 => NotFound,
            6 => AlreadyExists,
            7 => PermissionDenied,
            8 => ResourceExhausted,
            9 => FailedPrecondition,
            10 => Aborted,
            11 => OutOfRange,
            12 => Unimplemented,
            13 => Internal,
            14 => Unavailable,
            15 => DataLoss,
            16 => Unauthenticated,
            _ => Unknown,
        })
    }
}
