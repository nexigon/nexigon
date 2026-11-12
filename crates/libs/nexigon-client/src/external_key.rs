//! TLS client authentication with keys held by external signers.

use nexigon_cert::ExternalKey;
use nexigon_cert::SignatureAlgorithm;
use rustls::SignatureScheme;
use rustls::pki_types::SubjectPublicKeyInfoDer;
use rustls::sign::Signer;
use rustls::sign::SigningKey;

/// Rustls signing key backed by an [`ExternalKey`].
#[derive(Debug)]
pub(crate) struct ExternalSigningKey(pub(crate) ExternalKey);

impl SigningKey for ExternalSigningKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        let scheme = match self.0.algorithm() {
            SignatureAlgorithm::EcdsaP256Sha256 => SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureAlgorithm::EcdsaP384Sha384 => SignatureScheme::ECDSA_NISTP384_SHA384,
        };
        offered.contains(&scheme).then(|| {
            Box::new(ExternalSigner {
                key: self.0.clone(),
                scheme,
            }) as Box<dyn Signer>
        })
    }

    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        Some(SubjectPublicKeyInfoDer::from(self.0.public_key_der()))
    }

    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        match self.0.algorithm() {
            SignatureAlgorithm::EcdsaP256Sha256 | SignatureAlgorithm::EcdsaP384Sha384 => {
                rustls::SignatureAlgorithm::ECDSA
            }
        }
    }
}

/// Rustls signer for one handshake.
#[derive(Debug)]
struct ExternalSigner {
    key: ExternalKey,
    scheme: SignatureScheme,
}

impl Signer for ExternalSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        // Rustls reports only the message of an error, so the causes are part of it.
        self.key.sign(message).map_err(|error| {
            let mut message = error.to_string();
            let mut source = error.source();
            while let Some(cause) = source {
                message.push_str(&format!(": {cause}"));
                source = cause.source();
            }
            rustls::Error::General(message)
        })
    }

    fn scheme(&self) -> SignatureScheme {
        self.scheme
    }
}
