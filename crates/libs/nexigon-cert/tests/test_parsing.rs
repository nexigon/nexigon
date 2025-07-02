use nexigon_cert::Certificate;

/// Test certificate in PEM format.
const TEST_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBdzCCAR6gAwIBAgIUVZIf2/2hnYWfZPXx+0iAZ/5WuCwwCgYIKoZIzj0EAwIw
ITEfMB0GA1UEAwwWcmNnZW4gc2VsZiBzaWduZWQgY2VydDAgFw03NTAxMDEwMDAw
MDBaGA80MDk2MDEwMTAwMDAwMFowITEfMB0GA1UEAwwWcmNnZW4gc2VsZiBzaWdu
ZWQgY2VydDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABD/+RLBF+9gewjruFTy/
4y0lvjT+QdXJbWsRSADvJBvAe9SaNaID8KYmXo+8HcrVZWvNuDixRbM5DWeCGi8P
ej+jMjAwMB0GA1UdDgQWBBThwd9+3SvcyM5XeNw03YaGlUh0UDAPBgNVHRMBAf8E
BTADAQEAMAoGCCqGSM49BAMCA0cAMEQCIAaO5LVY3DHIArxkL/QKQSCnsNDZNtcJ
mdmQLWVrntBTAiBaLBx68yLNs9e7InnB3SvEUVxmObOFy3Y6XG+kNDz9iQ==
-----END CERTIFICATE-----"#;

/// SHA1 fingerprint of the test certificate.
const TEST_SHA1: &str = "67:DA:DC:12:18:20:3D:9B:91:16:F7:C0:44:E9:68:6B:37:58:31:98";

/// SHA256 fingerprint of the test certificate.
const TEST_SHA256: &str = "DE:49:D9:AF:6D:1E:9E:88:B0:00:8A:36:8D:4F:10:1B:7B:54:99:35:2E:8D:27:44:35:6C:78:15:71:7D:02:FE";

#[test]
pub fn test_fingerprints() {
    let certificate = Certificate::parse_pem(TEST_PEM).unwrap();
    assert_eq!(format!("{}", certificate.sha1_fingerprint()), TEST_SHA1);
    assert_eq!(format!("{}", certificate.sha256_fingerprint()), TEST_SHA256);
}
