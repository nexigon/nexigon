//! Device keys held by external signers.
//!
//! An [`ExternalKey`] combines the public key of a device key with a [`Signer`] that
//! signs messages with the corresponding private key, for example in a TPM, a PKCS#11
//! token, or device firmware. The private key never needs to leave the signer. Every
//! signature is verified with the public key, so a misbehaving signer fails with an
//! [`InvalidSignatureError`] instead of producing unusable signatures.
//! [`generate_self_signed_certificate_for`] creates a device certificate for such a key.

use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;

use x509_cert::der::Decode;
use x509_cert::der::Encode;
use x509_cert::der::asn1::ObjectIdentifier;
use x509_cert::spki::SubjectPublicKeyInfoOwned;

use crate::Certificate;

/// Signs messages with a private key that is not available to the caller.
pub trait Signer: Send + Sync + fmt::Debug {
    /// Sign a message with the signature algorithm of the key.
    ///
    /// The signer hashes the message itself and returns a DER-encoded ECDSA signature.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, SignError>;
}

/// Error reported by a [`Signer`].
pub type SignError = Box<dyn std::error::Error + Send + Sync>;

/// Signature algorithm of an [`ExternalKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureAlgorithm {
    /// ECDSA with curve P-256 and SHA-256.
    EcdsaP256Sha256,
    /// ECDSA with curve P-384 and SHA-384.
    EcdsaP384Sha384,
}

/// Device key whose private key is held by a [`Signer`].
#[derive(Debug, Clone)]
pub struct ExternalKey {
    /// DER-encoded subject public key info.
    public_key_der: Vec<u8>,
    /// Public key without the algorithm identifier, an uncompressed curve point.
    public_key_raw: Vec<u8>,
    /// Signature algorithm derived from the public key.
    algorithm: SignatureAlgorithm,
    /// Signer holding the private key.
    signer: Arc<dyn Signer>,
}

impl ExternalKey {
    /// Create a key from its DER-encoded subject public key info and its signer.
    ///
    /// Supports ECDSA keys on the curves P-256 and P-384 with uncompressed points.
    pub fn new(
        public_key_der: &[u8],
        signer: Arc<dyn Signer>,
    ) -> Result<Self, InvalidPublicKeyError> {
        let public_key = SubjectPublicKeyInfoOwned::from_der(public_key_der)
            .map_err(InvalidPublicKeyError::Malformed)?;
        if public_key.algorithm.oid != ID_EC_PUBLIC_KEY {
            return Err(InvalidPublicKeyError::Unsupported);
        }
        let curve = public_key
            .algorithm
            .parameters
            .as_ref()
            .and_then(|parameters| parameters.decode_as::<ObjectIdentifier>().ok());
        let (algorithm, point_len) = match curve {
            Some(SECP_256_R_1) => (SignatureAlgorithm::EcdsaP256Sha256, 65),
            Some(SECP_384_R_1) => (SignatureAlgorithm::EcdsaP384Sha384, 97),
            _ => return Err(InvalidPublicKeyError::Unsupported),
        };
        let public_key_raw = public_key.subject_public_key.raw_bytes().to_vec();
        if public_key_raw.len() != point_len || public_key_raw[0] != 0x04 {
            return Err(InvalidPublicKeyError::Unsupported);
        }
        Ok(Self {
            // Re-encoding yields the canonical form, which certificates contain.
            public_key_der: public_key
                .to_der()
                .expect("decoded public key can be encoded"),
            public_key_raw,
            algorithm,
            signer,
        })
    }

    /// DER-encoded subject public key info.
    pub fn public_key_der(&self) -> &[u8] {
        &self.public_key_der
    }

    /// Signature algorithm of the key.
    pub fn algorithm(&self) -> SignatureAlgorithm {
        self.algorithm
    }

    /// Sign a message with the key and verify the signature, see [`Signer::sign`].
    pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>, SignError> {
        let signature = self.signer.sign(message)?;
        let verification = match self.algorithm {
            SignatureAlgorithm::EcdsaP256Sha256 => &ring::signature::ECDSA_P256_SHA256_ASN1,
            SignatureAlgorithm::EcdsaP384Sha384 => &ring::signature::ECDSA_P384_SHA384_ASN1,
        };
        ring::signature::UnparsedPublicKey::new(verification, &self.public_key_raw)
            .verify(message, &signature)
            .map_err(|_| InvalidSignatureError(self.algorithm))?;
        Ok(signature)
    }
}

/// Generate a self-signed certificate for an external key.
///
/// The signer signs the certificate. Fails with the signer's error.
pub fn generate_self_signed_certificate_for(key: &ExternalKey) -> Result<Certificate, SignError> {
    let error = Arc::new(Mutex::new(None));
    let key_pair = rcgen::KeyPair::from_remote(Box::new(RemoteKey {
        key: key.clone(),
        error: error.clone(),
    }))
    .expect("remote key pairs are infallible");
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())
        .expect("certificate parameters without names are valid");
    // The default serial number derives from the public key. A random one keeps
    // certificates for the same key distinct, also with deterministic signatures,
    // so a key can obtain a new certificate after its old one has been revoked.
    let mut serial_number = [0; 20];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut serial_number)
        .expect("system random number generator is available");
    serial_number[0] &= 0x7f;
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial_number));
    let certificate = match params.self_signed(&key_pair) {
        Ok(certificate) => certificate,
        Err(rcgen::Error::RemoteKeyError) => {
            return Err(error
                .lock()
                .expect("signer does not panic while holding the lock")
                .take()
                .expect("signer error has been recorded"));
        }
        Err(error) => {
            unreachable!("only signing can fail for fixed certificate parameters: {error}")
        }
    };
    Ok(Certificate::parse_der(certificate.der()).expect("generated certificate is valid"))
}

/// Signature that does not verify with the public key of its [`ExternalKey`].
#[derive(Debug)]
pub struct InvalidSignatureError(SignatureAlgorithm);

impl fmt::Display for InvalidSignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hash = match self.0 {
            SignatureAlgorithm::EcdsaP256Sha256 => "SHA-256",
            SignatureAlgorithm::EcdsaP384Sha384 => "SHA-384",
        };
        write!(
            f,
            "signature does not verify with the public key, expected a DER-encoded ECDSA \
             signature over the {hash} hash of the message"
        )
    }
}

impl std::error::Error for InvalidSignatureError {}

/// Invalid public key error.
#[derive(Debug)]
#[non_exhaustive]
pub enum InvalidPublicKeyError {
    /// The public key is not a DER-encoded subject public key info.
    Malformed(x509_cert::der::Error),
    /// The key is neither an ECDSA P-256 nor an ECDSA P-384 key with an uncompressed
    /// point.
    Unsupported,
}

impl fmt::Display for InvalidPublicKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(_) => f.write_str("malformed DER-encoded subject public key info"),
            Self::Unsupported => f.write_str(
                "unsupported public key, expected an ECDSA P-256 or P-384 key with an \
                 uncompressed point",
            ),
        }
    }
}

impl std::error::Error for InvalidPublicKeyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Malformed(error) => Some(error),
            Self::Unsupported => None,
        }
    }
}

/// Object identifier of elliptic curve public keys.
const ID_EC_PUBLIC_KEY: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");
/// Object identifier of the curve P-256.
const SECP_256_R_1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.3.1.7");
/// Object identifier of the curve P-384.
const SECP_384_R_1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.132.0.34");

/// Adapter that lets `rcgen` sign with an external key.
struct RemoteKey {
    key: ExternalKey,
    /// Signer error, which `rcgen` cannot carry.
    error: Arc<Mutex<Option<SignError>>>,
}

impl rcgen::RemoteKeyPair for RemoteKey {
    fn public_key(&self) -> &[u8] {
        &self.key.public_key_raw
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        self.key.sign(message).map_err(|error| {
            *self
                .error
                .lock()
                .expect("signer does not panic while holding the lock") = Some(error);
            rcgen::Error::RemoteKeyError
        })
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        match self.key.algorithm {
            SignatureAlgorithm::EcdsaP256Sha256 => &rcgen::PKCS_ECDSA_P256_SHA256,
            SignatureAlgorithm::EcdsaP384Sha384 => &rcgen::PKCS_ECDSA_P384_SHA384,
        }
    }
}
