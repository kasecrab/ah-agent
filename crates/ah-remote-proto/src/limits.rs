//! The numbers both halves have to agree on, and why each one is what it is.

/// Protocol version. A peer that does not recognise it says so and stops,
/// rather than guessing at a frame whose meaning may have moved.
pub const PROTO: u8 = 1;

/// Largest frame a publisher will build, before sealing. Deltas are gathered
/// until this or [`FLUSH_MS`] runs out, whichever comes first.
pub const MAX_FRAME_BYTES: usize = 16 * 1024;

/// Longest a single event may be. A 2 MB diff would not fit the relay's row
/// limit and would cost a noticeable share of a day's budget to carry, so the
/// tail is replaced with a line saying how much was left behind.
pub const MAX_EVENT_BYTES: usize = 64 * 1024;

/// How long text deltas are gathered before a frame goes out. Long enough that
/// a chatty model costs tens of frames a minute rather than thousands, short
/// enough that a reader cannot tell.
pub const FLUSH_MS: u64 = 200;

/// Largest frame the relay will accept at all. Well above `MAX_FRAME_BYTES`,
/// so a legitimate burst is never refused, and well below the 2 MB a Durable
/// Object can store in one row.
pub const MAX_RELAY_FRAME_BYTES: usize = 128 * 1024;

/// Frames the relay keeps per hub. A phone that has been away longer than this
/// is told there is a gap and asks for a fresh snapshot instead.
pub const LOG_KEEP: u64 = 20_000;

/// How long the relay keeps a frame regardless of count: seven days.
pub const LOG_RETAIN_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// How far apart the two clocks may be before a connect signature is refused,
/// five minutes either way. The only place in the protocol where a clock
/// matters at all.
pub const SKEW_MS: u64 = 5 * 60 * 1000;

/// Phones attached to one hub at once. The oldest is closed to make room.
pub const MAX_DEVICES: usize = 4;

/// The desktop was replaced by another desktop on the same pairing.
pub const CLOSE_REPLACED: u16 = 4010;

/// A second desktop arrived while this one was still answering. The newcomer
/// is the one turned away: a live session is not interrupted by whoever
/// happens to connect next.
pub const CLOSE_BUSY: u16 = 4011;

/// This phone was closed to make room for a newer one.
pub const CLOSE_EVICTED: u16 = 4009;

/// The pairing was revoked. Not worth retrying, ever.
pub const CLOSE_REVOKED: u16 = 4012;
