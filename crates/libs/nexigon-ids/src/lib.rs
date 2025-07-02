// cspell:ignore NTRU
//! Unique identifiers for entities and access tokens.
//!
//! Within Nexigon, entities are identified by *ids* of the form `<tag>_<raw>` where
//! `<tag>` indicates the type of the entity and `<raw>` is a base 58 numeric string
//! uniquely identifying an entity of the respective type. We refer to `<raw>` as a
//! *raw id*.
//!
//! Internally, entities in the database may have sequential numeric ids, however,
//! Nexigon's APIs work exclusively with entity ids as defined here. These ids have the
//! advantage that they cannot be guessed and that they carry a type prefix enabling a
//! user/developer to immediately see what type of entity is identified. This design has
//! been inspired by Stripe's ids.[^1]
//!
//! Ids can be randomly generated or derived from a sequence of bytes, e.g., a hash, using
//! a variation of the NTRU Prime encoding.[^2]
//!
//! [^1]: <https://dev.to/stripe/designing-apis-for-humans-object-ids-3o5a>
//! [^2]: <https://carlmastrangelo.com/blog/a-better-base-58-encoding>
//!
//! We use the same schema for secret ids, e.g., user and project access tokens. As a
//! security precaution, the [`Display`][std::fmt::Display] and [`Debug`] implementations
//! of secret ids will not include the raw id.

use std::sync::Arc;

use rand::RngCore;
use sha2::Sha512_256;
use sha2::digest::Digest;

mod encoding;

/// Base 58 alphabet for ids.
const ALPHABET_BASE58: &[char] = &[
    '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'J', 'K',
    'L', 'M', 'N', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', 'a', 'b', 'c', 'd', 'e',
    'f', 'g', 'h', 'i', 'j', 'k', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y',
    'z',
];

/// Base 36 alphabet.
const ALPHABET_BASE36: &[char; 36] = &[
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i',
    'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z',
];

/// Lookup table for base 58 digits.
const BASE58_DIGITS: &[u8] = &[
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 0, 1, 2, 3, 4, 5, 6, 7, 8, 255, 255,
    255, 255, 255, 255, 255, 9, 10, 11, 12, 13, 14, 15, 16, 255, 17, 18, 19, 20, 21, 255, 22, 23,
    24, 25, 26, 27, 28, 29, 30, 31, 32, 255, 255, 255, 255, 255, 255, 33, 34, 35, 36, 37, 38, 39,
    40, 41, 42, 43, 255, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 255, 255, 255,
    255, 255,
];

/// Numeric base of ids (`58`).
const BASE: u32 = 58;

/// Tagged id.
pub trait Id {
    /// Tag of the id.
    fn tag(&self) -> Tag;

    /// Raw id.
    fn raw(&self) -> &RawId;

    /// Convert the id to a string.
    fn stringify(&self) -> String;

    /// Compute a base 36 *fingerprint* of the id.
    fn base36_fingerprint(&self) -> String {
        let mut hasher = sha2::Sha256::new();
        hasher.update(self.tag().as_str());
        hasher.update("_");
        hasher.update(self.raw().as_str());
        let mut out = String::new();
        encoding::encode(
            &mut out,
            ALPHABET_BASE36,
            u16::MAX.into(),
            &hasher.finalize(),
        );
        out
    }
}

/// Marker trait for public ids.
pub trait PublicId: Id {}

/// Marker trait for secret ids.
pub trait SecretId: Id {}

/// Random generation of ids.
pub trait Generate {
    /// Generate a random id.
    fn generate() -> Self;
}

/// Auxiliary macro for implementing marker traits.
macro_rules! impl_marker_trait {
    ($name:ident, false) => {
        impl PublicId for $name {}
    };
    ($name:ident, true) => {
        impl SecretId for $name {}
    };
}

/// Auxiliary macro for defining id types.
macro_rules! define_types {
    ($(
        $(#[$meta:meta])*
        $name:ident => (
            $string:literal,
            $size:literal,
            secret = $secret:tt
        ),
    )*) => {
        /// Id tag.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Tag {
            $(
                $(#[$meta])*
                $name,
            )*
        }

        impl Tag {
            /// String representation of the tag.
            pub fn as_str(&self) -> &'static str {
                match self {
                    $(
                        Self::$name => $string,
                    )*
                }
            }

            /// Size of the tag.
            pub fn tag_size(&self) -> usize {
                self.as_str().len()
            }

            /// Size of the respective raw id.
            pub fn raw_size(&self) -> usize {
                match self {
                    $(
                        Self::$name => $size,
                    )*
                }
            }

            /// Size of the entire id including the tag.
            pub fn id_size(&self) -> usize {
                self.tag_size() + 1 + self.raw_size()
            }

            /// Indicates whether an id with the tag is a secret.
            pub fn is_secret(&self) -> bool {
                match self {
                    $(
                        Self::$name => $secret,
                    )*
                }
            }

            /// Generate an id with the given tag.
            pub fn generate(&self) -> AnyId {
                match self {
                    $(
                        Self::$name => AnyId::$name(ids::$name::generate()),
                    )*
                }
            }
        }

        impl std::str::FromStr for Tag {
            type Err = errors::InvalidTagError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $(
                        $string => Ok(Self::$name),
                    )*
                    _ => Err(errors::InvalidTagError::new())
                }
            }
        }

        /// Polymorphic id.
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum AnyId {
            $(
                $(#[$meta])*
                $name(ids::$name),
            )*
        }

        impl Id for AnyId {
            fn tag(&self) -> Tag {
                match self {
                    $(
                        Self::$name(id) => id.tag(),
                    )*
                }
            }

            fn raw(&self) -> &RawId {
                match self {
                    $(
                        Self::$name(id) => id.raw(),
                    )*
                }
            }

            fn stringify(&self) -> String {
                match self {
                    $(
                        Self::$name(id) => id.stringify(),
                    )*
                }
            }
        }

        impl serde::Serialize for AnyId {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self {
                    $(
                        Self::$name(id) => id.serialize(serializer),
                    )*
                }
            }
        }

        impl<'de> serde::Deserialize<'de> for AnyId {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                use serde::de::Error;
                let string = String::deserialize(deserializer)?;
                string.parse().map_err(|_| {
                    D::Error::invalid_value(
                        serde::de::Unexpected::Str(&string),
                        &"expected any id"
                    )
                })
            }
        }

        impl std::str::FromStr for AnyId {
            type Err = errors::InvalidIdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                // cspell:ignore rsplit
                if let Some((tag, raw)) = s.rsplit_once("_") {
                    let raw = RawId::from_str(raw)?;
                    match tag {
                        $(
                            $string => Ok(Self::$name(raw.try_into()?)),
                        )*
                        _ => Err(errors::InvalidIdError::new("unknown tag"))
                    }
                } else {
                    Err(errors::InvalidIdError::new(
                        "id does not start with a tag"
                    ))
                }
            }
        }

        /// Id types.
        pub mod ids {
            use super::*;

            $(
                $(#[$meta])*
                #[derive(Clone, PartialEq, Eq, Hash)]
                pub struct $name {
                    /// Raw id.
                    raw: RawId,
                }

                impl $name {
                    /// Create an id from the provided raw id without checking its size.
                    pub fn from_raw_unchecked(raw: RawId) -> Self {
                        Self { raw }
                    }
                }

                impl std::fmt::Debug for $name {
                    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        if $secret {
                            f.debug_struct(stringify!($name)).finish_non_exhaustive()
                        } else {
                            f.debug_struct(stringify!($name)).field("raw", &self.raw).finish()
                        }
                    }
                }

                impl_marker_trait!($name, $secret);

                impl Id for $name {
                    fn tag(&self) -> Tag {
                        Tag::$name
                    }

                    fn raw(&self) -> &RawId {
                        &self.raw
                    }

                    fn stringify(&self) -> String {
                        let mut string = String::with_capacity($string.len() + 1 + $size);
                        string.push_str($string);
                        string.push('_');
                        string.push_str(self.raw.as_str());
                        string
                    }
                }

                impl Generate for $name {
                    fn generate() -> Self {
                        Self::from_raw_unchecked(RawId::generate($size))
                    }
                }

                impl serde::Serialize for $name {
                    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                        serializer.serialize_str(&self.stringify())
                    }
                }

                impl<'de> serde::Deserialize<'de> for $name {
                    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                        use serde::de::Error;
                        let string = String::deserialize(deserializer)?;
                        string.parse().map_err(|_| {
                            D::Error::invalid_value(
                                serde::de::Unexpected::Str(&string),
                                &concat!("expected id with tag `", $string, "`")
                            )
                        })
                    }
                }

                impl std::fmt::Display for $name {
                    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        f.write_str($string)?;
                        f.write_str("_")?;
                        if $secret {
                            f.write_str("<redacted>")?;
                        } else {
                            f.write_str(self.raw().as_str())?;
                        }
                        Ok(())
                    }
                }

                impl std::str::FromStr for $name {
                    type Err = errors::InvalidIdError;

                    fn from_str(s: &str) -> Result<Self, Self::Err> {
                        if let Some(raw) = s.strip_prefix(concat!($string, "_")) {
                            Self::try_from(RawId::from_str(raw)?)
                        } else {
                            Err(errors::InvalidIdError::new(
                                concat!("invalid prefix (expected: `", $string, "_`)")
                            ))
                        }
                    }
                }

                impl From<$name> for AnyId {
                    fn from(id: $name) -> Self {
                        Self::$name(id)
                    }
                }

                impl From<$name> for RawId {
                    fn from(id: $name) -> RawId {
                        id.raw
                    }
                }

                impl TryFrom<RawId> for $name {
                    type Error = errors::InvalidIdError;

                    fn try_from(raw: RawId) -> Result<Self, Self::Error> {
                        if raw.as_str().len() == $size {
                            Ok(Self { raw })
                        } else {
                            Err(errors::InvalidIdError::new(concat!(
                                "invalid size of raw id (expected: ", stringify!($size), ")"
                            )))
                        }
                    }
                }
            )*
        }
    };
}

define_types! {
    /// Cluster node id (globally unique).
    ///
    /// Uniquely identifies a cluster node within the system.
    ClusterNodeId => ("cluster_node", 22, secret = false),

    /// User id (globally unique).
    ///
    /// Uniquely identifies a user within the system.
    UserId => ("u", 22, secret = false),
    /// User secret access token (globally unique).
    ///
    /// Used in-place of the password for login with the API and client.
    UserToken => ("u_sk", 66, secret = true),
    /// User access token id (globally unique).
    ///
    /// The first 22 characters of the respective secret access token.
    UserTokenId => ("u_pk", 22, secret = false),
    /// User session token (globally unique).
    UserSessionToken => ("u_session_sk", 66, secret = true),
    /// User session id (globally unique).
    UserSessionId => ("u_session_pk", 22, secret = false),

    /// Project id (globally unique).
    ///
    /// Uniquely identifies a project within the system.
    ProjectId => ("p", 22, secret = false),
    /// Project secret access token (globally unique).
    ///
    /// Used by devices to connect to the project.
    ProjectToken => ("p_sk", 44, secret = true),
    /// Project access token id (globally unique).
    ///
    /// The first 22 characters of the respective secret access token.
    ProjectTokenId => ("p_pk", 22, secret = false),

    /// Deployment token (globally unique).
    ///
    /// Used by devices to connect to a project.
    DeploymentToken => ("deployment", 66, secret = true),
    /// Deployment token id (globally unique).
    ///
    /// The first 22 characters of the respective deployment token.
    DeploymentTokenId => ("deployment_id", 22, secret = false),

    /// Device id (globally unique).
    ///
    /// Uniquely identifies a device within the system.
    DeviceId => ("d", 22, secret = false),
    /// Device fingerprint (unique per project).
    ///
    /// Generated by the device as a unique identifier for itself.
    ///
    /// Used for authenticating the device together with a project token.
    DeviceFingerprint => ("d_sk", 44, secret = true),
    /// Device fingerprint id (unique per project).
    DeviceFingerprintId => ("d_pk", 22, secret = false),
    /// Device certificate id (globally unique).
    DeviceCertificateId => ("d_c", 22, secret = false),
    /// Device connection id (globally unique).
    DeviceConnectionId => ("d_conn", 22, secret = false),
    /// Device event id (unique per device).
    DeviceEventId => ("d_ev", 22, secret = false),

    /// Repository id (globally unique).
    RepositoryId => ("repo", 22, secret = false),
    /// Repository asset it (globally unique).
    RepositoryAssetId => ("repo_a", 22, secret = false),

    /// Package id (globally unique).
    PackageId => ("pkg", 22, secret = false),
    /// Package version id (globally unique).
    PackageVersionId => ("pkg_v", 22, secret = false),

    /// Job id (globally unique).
    JobId => ("job", 22, secret = false),
}

/// Check whether a character is a base 58 digit.
fn is_base58_digit(c: char) -> bool {
    if u32::from(c) < 128 {
        BASE58_DIGITS[c as usize] != u8::MAX
    } else {
        false
    }
}

impl std::fmt::Display for AnyId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let tag = self.tag();
        f.write_str(tag.as_str())?;
        f.write_str("_")?;
        if tag.is_secret() {
            f.write_str("<redacted>")?;
        } else {
            f.write_str(self.raw().as_str())?;
        }
        Ok(())
    }
}

impl ids::UserToken {
    /// Id of the token.
    pub fn token_id(&self) -> ids::UserTokenId {
        ids::UserTokenId::from_raw_unchecked(RawId::new_unchecked(
            &self.raw().as_str()[..Tag::UserTokenId.raw_size()],
        ))
    }
}

impl ids::DeploymentToken {
    /// Id of the token.
    pub fn token_id(&self) -> ids::DeploymentTokenId {
        ids::DeploymentTokenId::from_raw_unchecked(RawId::new_unchecked(
            &self.raw().as_str()[..Tag::ProjectTokenId.raw_size()],
        ))
    }
}

impl ids::ProjectToken {
    /// Id of the token.
    pub fn token_id(&self) -> ids::ProjectTokenId {
        ids::ProjectTokenId::from_raw_unchecked(RawId::new_unchecked(
            &self.raw().as_str()[..Tag::ProjectTokenId.raw_size()],
        ))
    }
}

impl ids::UserSessionToken {
    /// Id of the token.
    pub fn token_id(&self) -> ids::UserSessionId {
        ids::UserSessionId::from_raw_unchecked(RawId::new_unchecked(
            &self.raw().as_str()[..Tag::UserSessionId.raw_size()],
        ))
    }
}

impl ids::DeviceFingerprint {
    /// Id of the fingerprint.
    pub fn fingerprint_id(&self) -> ids::DeviceFingerprintId {
        ids::DeviceFingerprintId::from_raw_unchecked(RawId::new_unchecked(
            &self.raw().as_str()[..Tag::DeviceFingerprintId.raw_size()],
        ))
    }

    /// Create a fingerprint from the given data.
    pub fn from_data(data: &[u8]) -> ids::DeviceFingerprint {
        let mut hasher = Sha512_256::new();
        hasher.update(data);
        Self::from_raw_unchecked(RawId::from_bytes(&hasher.finalize()))
    }
}

/// Raw id without a tag.
///
/// The internal representation is a base 58 numeric string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RawId {
    str: Arc<str>,
}

impl RawId {
    /// Create a raw id from the given string without checking it for validity.
    fn new_unchecked(str: impl Into<Arc<str>>) -> Self {
        Self { str: str.into() }
    }

    /// Create a raw id from the given bytes.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        /// Limit parameter of the NTRU Prime encoding.
        const LIMIT: u32 = u16::MAX as u32;

        fn emit_digit(str: &mut String, digit: u32, base: u32) -> (u32, u32) {
            str.push(ALPHABET_BASE58[(digit % BASE) as usize]);
            (digit / BASE, base.div_ceil(BASE))
        }

        fn from_bytes_rec(str: &mut String, bytes: &[u8]) -> (u32, u32) {
            match bytes.len() {
                0 => (0, 0),
                1 => (bytes[0] as u32, 256),
                _ => {
                    let mid = bytes.len() / 2;
                    let (first_digit, first_base) = from_bytes_rec(str, &bytes[..mid]);
                    let (second_digit, second_base) = from_bytes_rec(str, &bytes[mid..]);
                    let mut base = first_base * second_base;
                    let mut digit = first_digit + second_digit * first_base;
                    while base >= LIMIT {
                        (digit, base) = emit_digit(str, digit, base);
                    }
                    (digit, base)
                }
            }
        }

        let mut str = String::new();
        let (mut digit, mut base) = from_bytes_rec(&mut str, bytes);
        while base > 1 {
            (digit, base) = emit_digit(&mut str, digit, base);
        }
        Self::new_unchecked(str)
    }

    /// Generate a random raw id with the given size.
    pub(crate) fn generate(size: usize) -> Self {
        const MASK: u32 = BASE.next_power_of_two() - 1;

        // We use rejection sampling to get a uniform distribution over the raw id space. We use a
        // buffer to avoid calling the random number generator in the hot loop.
        let mut rng = rand::rng();
        let mut buffer = [0; 64];
        let mut str = String::with_capacity(size);
        'outer: while str.len() < size {
            rng.fill_bytes(&mut buffer);
            for byte in &buffer {
                let digit = (*byte as u32) & MASK;
                if digit < BASE {
                    str.push(ALPHABET_BASE58[digit as usize]);
                    if str.len() >= size {
                        break 'outer;
                    }
                }
            }
        }
        Self::new_unchecked(str)
    }

    /// String representation of the raw id.
    pub fn as_str(&self) -> &str {
        &self.str
    }
}

impl std::str::FromStr for RawId {
    type Err = errors::InvalidIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.chars().all(is_base58_digit) {
            Ok(Self { str: s.into() })
        } else {
            Err(errors::InvalidIdError::new("invalid digit in raw id"))
        }
    }
}

impl AsRef<str> for RawId {
    fn as_ref(&self) -> &str {
        &self.str
    }
}

/// Error types.
pub mod errors {

    /// Invalid tag error.
    #[derive(Debug)]
    pub struct InvalidTagError(pub(super) ());

    impl InvalidTagError {
        /// Create an error.
        pub(crate) fn new() -> Self {
            Self(())
        }
    }

    impl std::fmt::Display for InvalidTagError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("invalid id tag")
        }
    }

    impl std::error::Error for InvalidTagError {}

    /// Invalid id error.
    #[derive(Debug)]
    pub struct InvalidIdError {
        /// Reason why the id is invalid.
        reason: &'static str,
    }

    impl InvalidIdError {
        /// Create an error with the given reason.
        pub(crate) fn new(reason: &'static str) -> Self {
            Self { reason }
        }
    }

    impl std::fmt::Display for InvalidIdError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.reason)
        }
    }

    impl std::error::Error for InvalidIdError {}
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    pub fn test_raw_id_generation() {
        for size in 0..256 {
            assert_eq!(RawId::generate(size).as_str().len(), size);
        }
    }

    #[test]
    pub fn test_raw_id_parsing() {
        assert!(RawId::from_str("abc0").is_err());
        assert!(RawId::from_str("abc123").is_ok());
    }

    #[test]
    pub fn test_id_parsing() {
        // cspell:disable-next-line
        const TEST_ID: &str = "u_ZjRcffdyfXutC6XUAkswBx";
        // cspell:disable-next-line
        const TEST_ID_RAW: &str = "ZjRcffdyfXutC6XUAkswBx";
        let user_id = ids::UserId::from_str(TEST_ID).unwrap();
        assert_eq!(user_id.raw().as_str(), TEST_ID_RAW);
        assert_eq!(
            AnyId::from_str(TEST_ID).unwrap(),
            AnyId::from(ids::UserId::from_str(TEST_ID).unwrap())
        );
    }

    #[test]
    pub fn test_device_fingerprint() {
        assert_eq!(
            ids::DeviceFingerprint::from_data(b"abc")
                .raw()
                .as_str()
                .len(),
            Tag::DeviceFingerprint.raw_size()
        );
    }
}
