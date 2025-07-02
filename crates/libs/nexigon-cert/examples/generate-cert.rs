use nexigon_cert::generate_self_signed_certificate;

pub fn main() {
    let args = std::env::args().skip(1).collect::<Vec<String>>();
    let [cert_path, key_path] = args.as_slice() else {
        eprintln!("usage: generate-cert <cert-path> <key-path>");
        std::process::exit(1);
    };
    let (certificate, key) = generate_self_signed_certificate();
    std::fs::write(cert_path, certificate.to_pem()).unwrap();
    std::fs::write(key_path, key).unwrap();
}
