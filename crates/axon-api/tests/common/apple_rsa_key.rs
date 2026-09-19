// One fresh RSA test key per test process, never written to disk.
struct RsaTestKey {
    pem: String,
    n: String,
    e: String,
}

fn rsa_key() -> &'static RsaTestKey {
    use aws_lc_rs::{encoding::AsDer, signature::KeyPair};
    use base64::Engine;
    static KEY: std::sync::OnceLock<RsaTestKey> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        let pair = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let der: aws_lc_rs::encoding::Pkcs8V1Der<'static> = pair.as_der().unwrap();
        let public = pair.public_key();
        let base64 = &base64::engine::general_purpose::STANDARD;
        let url64 = &base64::engine::general_purpose::URL_SAFE_NO_PAD;
        RsaTestKey {
            pem: format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", base64.encode(der.as_ref())),
            n: url64.encode(public.modulus().big_endian_without_leading_zero()),
            e: url64.encode(public.exponent().big_endian_without_leading_zero()),
        }
    })
}
