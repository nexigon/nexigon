// cspell:ignore rcgen
//! X509 device certificates.

use std::fmt::Write;
use std::str::FromStr;

use x509_cert::der::Decode;
use x509_cert::der::DecodePem;
use x509_cert::der::Encode;
use x509_cert::der::EncodePem;

/// X509 certificate.
#[derive(Debug, Clone)]
pub struct Certificate {
    /// Inner certificate from [`x509_cert`].
    inner: x509_cert::Certificate,
}

impl Certificate {
    /// Parse a certificate in PEM format.
    pub fn parse_pem(pem: &str) -> Result<Self, InvalidCertificateError> {
        x509_cert::Certificate::from_pem(pem)
            .map_err(InvalidCertificateError::new)
            .map(|inner| Self { inner })
    }

    /// Parse a certificate in DER format.
    pub fn parse_der(der: &[u8]) -> Result<Self, InvalidCertificateError> {
        x509_cert::Certificate::from_der(der)
            .map_err(InvalidCertificateError::new)
            .map(|inner| Self { inner })
    }

    /// SHA1 fingerprint of the certificate.
    pub fn sha1_fingerprint(&self) -> Sha1Fingerprint {
        use sha1::Digest;
        let mut hasher = sha1::Sha1::new();
        hasher.update(self.inner.to_der().unwrap());
        Fingerprint::new(hasher.finalize().into())
    }

    /// SHA256 fingerprint of the certificate.
    pub fn sha256_fingerprint(&self) -> Sha256Fingerprint {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(self.inner.to_der().unwrap());
        Fingerprint::new(hasher.finalize().into())
    }

    /// Encode the certificate in PEM format.
    pub fn to_pem(&self) -> String {
        self.inner
            .to_pem(x509_cert::der::pem::LineEnding::LF)
            .expect("certificate is valid")
    }

    /// Encode the certificate in DER format.
    pub fn to_der(&self) -> Vec<u8> {
        self.inner.to_der().expect("certificate is valid")
    }
}

/// Generate a self-signed certificate and key in PEM format.
pub fn generate_self_signed_certificate() -> (Certificate, String) {
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed([]).expect("should not fail");
    (
        Certificate::parse_pem(&cert.pem()).expect("certificate is valid"),
        key_pair.serialize_pem(),
    )
}

/// Certificate fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Fingerprint<T> {
    digest: T,
}

impl<T> Fingerprint<T> {
    /// Create a new fingerprint with the given digest.
    pub fn new(digest: T) -> Self {
        Self { digest }
    }

    /// Convert the fingerprint to the underlying digest.
    pub fn into_digest(self) -> T {
        self.digest
    }
}

impl<T: AsRef<[u8]>> AsRef<[u8]> for Fingerprint<T> {
    fn as_ref(&self) -> &[u8] {
        self.digest.as_ref()
    }
}

impl<T: AsRef<[u8]>> std::fmt::Display for Fingerprint<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (idx, byte) in self.digest.as_ref().iter().enumerate() {
            if idx > 0 {
                f.write_char(':')?;
            }
            f.write_fmt(format_args!("{:02X}", *byte))?;
        }
        Ok(())
    }
}

impl<T: AsRef<[u8]>> serde::Serialize for Fingerprint<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&format!("{self}"))
    }
}

impl<'de, const N: usize> serde::Deserialize<'de> for Fingerprint<[u8; N]> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let string = String::deserialize(deserializer)?;
        string.parse().map_err(|_| {
            <D::Error as serde::de::Error>::invalid_value(
                serde::de::Unexpected::Str(&string),
                &"certificate fingerprint",
            )
        })
    }
}

impl<const N: usize> FromStr for Fingerprint<[u8; N]> {
    type Err = InvalidFingerprintError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut digest = [0; N];
        let mut idx = 0;
        for digit in s.split(':') {
            let Ok(digit) = u8::from_str_radix(digit, 16) else {
                return Err(InvalidFingerprintError(()));
            };
            if idx < N {
                digest[idx] = digit;
                idx += 1;
            } else {
                return Err(InvalidFingerprintError(()));
            }
        }
        if idx < N {
            Err(InvalidFingerprintError(()))
        } else {
            Ok(Fingerprint::new(digest))
        }
    }
}

/// SHA1 fingerprint.
pub type Sha1Fingerprint = Fingerprint<[u8; 20]>;

/// SHA256 fingerprint.
pub type Sha256Fingerprint = Fingerprint<[u8; 32]>;

/// Invalid fingerprint error.
#[derive(Debug)]
pub struct InvalidFingerprintError(());

impl std::fmt::Display for InvalidFingerprintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid certificate fingerprint")
    }
}

impl std::error::Error for InvalidFingerprintError {}

/// Invalid certificate error.
#[derive(Debug)]
pub struct InvalidCertificateError {
    /// Inner error from [`x509_cert`].
    inner: x509_cert::der::Error,
}

impl InvalidCertificateError {
    /// Wrap an [`x509_cert`] error.
    fn new(inner: x509_cert::der::Error) -> Self {
        Self { inner }
    }
}

impl std::fmt::Display for InvalidCertificateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid x509 certificate")
    }
}

impl std::error::Error for InvalidCertificateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.inner)
    }
}
