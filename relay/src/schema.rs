//! What a hub keeps, and how it is kept from growing without end.

/// Created once, in the object's constructor. `SqlStorage::exec` is
/// synchronous and the constructor runs before any request is dispatched, so
/// the table is in place without needing the atomic window that the JavaScript
/// SDK's `blockConcurrencyWhile` buys — which this SDK does not expose at all.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS log (
  n    INTEGER PRIMARY KEY AUTOINCREMENT,
  ts   INTEGER NOT NULL,
  link TEXT    NOT NULL,
  seq  INTEGER NOT NULL,
  ct   TEXT    NOT NULL
);
CREATE TABLE IF NOT EXISTS nonces (
  n  TEXT PRIMARY KEY,
  ts INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS devices (
  id       TEXT PRIMARY KEY,
  role     TEXT    NOT NULL,
  first_ms INTEGER NOT NULL,
  last_ms  INTEGER NOT NULL,
  cursor   INTEGER NOT NULL DEFAULT 0
);
";

/// Frames kept per hub. A phone away for longer than this is told there is a
/// gap and asks the desktop for a fresh snapshot instead.
pub const LOG_KEEP: i64 = 20_000;

/// How much of a hub's log is kept, in bytes of ciphertext. A count alone is
/// not a bound: [`LOG_KEEP`] frames at the largest size the relay will take
/// would be gigabytes, and storage is what is being paid for.
pub const LOG_BYTES: i64 = 64 * 1024 * 1024;

/// How long a device row is kept after that device was last seen. It is
/// written on every connect and read by nothing once the socket is closed.
pub const DEVICE_KEEP_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// How long a frame is kept regardless of how few there are: seven days.
pub const LOG_RETAIN_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// How often the log is trimmed. Deleted rows count against the same daily
/// allowance as written ones, so this happens in batches and never per insert.
pub const PRUNE_EVERY: i64 = 256;

/// How long a used connect nonce is remembered. Twice the skew window, so a
/// signature is out of date before the relay stops recognising that it has
/// already been used once.
pub const NONCE_KEEP_MS: i64 = 2 * 5 * 60 * 1000;

/// Frames one hub may take in a day. The allowance being spent belongs to
/// whoever deployed this, so a pairing code that got out cannot quietly empty
/// it.
pub const FRAMES_PER_DAY: i64 = 40_000;
