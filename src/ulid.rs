//! ULID: a 128-bit identifier that sorts by creation time.
//!
//! ```text
//!  0                   4                   8                  12       15
//! +-------------------------+-----------------------------------------+
//! | 48 bits of milliseconds |            80 random bits               |
//! +-------------------------+-----------------------------------------+
//! ```
//!
//! Both halves are big-endian, which is the whole point: comparing two ULIDs
//! byte by byte compares their timestamps first, so `ORDER BY id` in SQLite —
//! which compares blobs with `memcmp` — is chronological order, and a range of
//! time is a contiguous range of keys. A UUIDv4 primary key gives neither.
//!
//! Written here instead of taken from the `ulid` crate because the spec is a
//! timestamp, ten random bytes and a base32 alphabet, and all three fit on one
//! screen. One deliberate difference from <https://github.com/ulid/spec>: there
//! is **no monotonic factory**. Two ULIDs made in the same millisecond sort
//! arbitrarily against each other rather than by the order they were made, and
//! nothing here keeps the state that fixing it would need. Add it when
//! something actually depends on within-millisecond ordering.

use std::fmt;
use std::fmt::Write;
use std::time::{SystemTime, UNIX_EPOCH};

/// Crockford's base32: the digits and the uppercase letters, minus `I`, `L`,
/// `O` and `U` — the four that are misread as `1`, `1`, `0` and each other.
const ALPHABET: [u8; 32] = *b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 128 bits at 5 bits per character, rounded up. The first character therefore
/// carries only 3 bits, so no ULID starts above `7`.
const TEXT_LENGTH: usize = 26;

/// Leading bytes holding the timestamp; the remaining 10 are random.
const TIMESTAMP_BYTES: usize = 6;

/// Largest millisecond count that fits in [`TIMESTAMP_BYTES`] — the year 10889.
const MAXIMUM_MILLISECONDS: u128 = (1 << (8 * TIMESTAMP_BYTES as u128)) - 1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("System clock reads before the Unix epoch")]
    Clock {
        #[source]
        source: std::time::SystemTimeError,
    },

    #[error("Timestamp {milliseconds} ms does not fit in the {TIMESTAMP_BYTES} bytes a ULID has")]
    Timestamp { milliseconds: u128 },

    #[error("Could not read random bytes from the operating system")]
    Random {
        #[source]
        source: getrandom::Error,
    },

    #[error("{text:?} is {characters} characters, and a ULID is {TEXT_LENGTH}")]
    TextLength { text: String, characters: usize },

    #[error("{text:?} contains {character:?}, which is not one of Crockford's 32 digits")]
    TextCharacter { text: String, character: char },

    #[error("{text:?} starts with {character:?}, and no ULID starts above '7'")]
    TextOverflow { text: String, character: char },
}

/// A ULID, stored as the 16 bytes that go into the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ulid([u8; 16]);

impl Ulid {
    /// Make one for right now.
    ///
    /// Fallible because both halves come from outside the process: the clock
    /// can read before 1970, and the operating system's random source can
    /// refuse. Neither is worth papering over — a ULID built from a wrong
    /// timestamp or from predictable bytes is a primary key that will collide
    /// or mis-sort later, far away from the cause.
    pub fn new() -> Result<Self, Error> {
        let milliseconds = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => elapsed.as_millis(),
            Err(source) => return Err(Error::Clock { source }),
        };
        if milliseconds > MAXIMUM_MILLISECONDS {
            return Err(Error::Timestamp { milliseconds });
        }

        let mut bytes = [0u8; 16];
        for (index, byte) in bytes[..TIMESTAMP_BYTES].iter_mut().enumerate() {
            *byte = (milliseconds >> (8 * (TIMESTAMP_BYTES - 1 - index))) as u8;
        }
        // Straight from the OS entropy source, no userspace generator to seed
        // or to keep in sync across processes.
        if let Err(source) = getrandom::fill(&mut bytes[TIMESTAMP_BYTES..]) {
            return Err(Error::Random { source });
        }
        Ok(Self(bytes))
    }

    /// The 16 bytes, for the `BLOB(16)` primary key.
    pub fn bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Rebuild one from the bytes read back out of the database.
    ///
    /// Infallible on purpose: every 16-byte pattern is a ULID. A timestamp far
    /// in the future or a run of zeros is a value, not a parse error, and the
    /// length is the caller's problem because that is where the useful context
    /// about *which* row is malformed lives.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Read one back from the 26-character text form [`Display`] writes.
    ///
    /// Case-insensitive, because Crockford's alphabet has no lowercase and a
    /// ULID that has been through a shell, a URL or a copy-paste may well
    /// arrive in it. `I`, `L` and `O` are *not* folded onto `1` and `0`,
    /// though: the alphabet leaves them out to keep a human from misreading one
    /// aloud, and quietly accepting the mistake would file a row under an
    /// identifier nobody can type twice.
    ///
    /// [`Display`]: fmt::Display
    pub fn parse(text: &str) -> Result<Self, Error> {
        let characters = text.chars().count();
        if characters != TEXT_LENGTH {
            return Err(Error::TextLength {
                text: text.to_string(),
                characters,
            });
        }

        let mut value: u128 = 0;
        for (index, character) in text.chars().enumerate() {
            let upper = character.to_ascii_uppercase();
            let mut digit = None;
            for (position, letter) in ALPHABET.iter().enumerate() {
                if char::from(*letter) == upper {
                    digit = Some(position as u128);
                }
            }
            let digit = match digit {
                Some(digit) => digit,
                None => {
                    return Err(Error::TextCharacter {
                        text: text.to_string(),
                        character,
                    });
                }
            };
            // 26 characters carry 130 bits and a ULID is 128, so the first one
            // has only 3 to spend. Anything above `7` there would shift its top
            // bits off the end and parse as a different ULID than it reads as.
            if index == 0 && digit > 0b111 {
                return Err(Error::TextOverflow {
                    text: text.to_string(),
                    character,
                });
            }
            value = (value << 5) | digit;
        }
        Ok(Self(value.to_be_bytes()))
    }

    /// Milliseconds since the Unix epoch, read back out of the leading bytes.
    ///
    /// The timestamp column is rendered from this rather than from a second
    /// clock reading, so a row's `created_at` can never disagree with the `id`
    /// sitting next to it.
    pub fn milliseconds(&self) -> u64 {
        let mut milliseconds = 0;
        for byte in &self.0[..TIMESTAMP_BYTES] {
            milliseconds = (milliseconds << 8) | u64::from(*byte);
        }
        milliseconds
    }
}

/// The 26-character text form, most significant bits first.
impl fmt::Display for Ulid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = u128::from_be_bytes(self.0);
        for index in 0..TEXT_LENGTH {
            let shift = 5 * (TEXT_LENGTH - 1 - index);
            formatter.write_char(char::from(ALPHABET[((value >> shift) & 0b11111) as usize]))?;
        }
        Ok(())
    }
}
