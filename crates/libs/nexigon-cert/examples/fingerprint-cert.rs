use nexigon_cert::Certificate;

pub fn main() {
    let Some(cert_path) = std::env::args().nth(1) else {
        eprintln!("usage: fingerprint-cert <cert-path>");
        std::process::exit(1);
    };
    match std::fs::read_to_string(&cert_path) {
        Ok(pem) => {
            let Ok(certificate) = Certificate::parse_pem(&pem) else {
                eprintln!("error parsing certificate");
                std::process::exit(1)
            };
            println!("SHA1 = {}", certificate.sha1_fingerprint());
            println!("SHA256 = {}", certificate.sha256_fingerprint());
        }
        Err(error) => {
            eprintln!("error reading certificate from {cert_path:?}: {error}");
            std::process::exit(1)
        }
    }
}
