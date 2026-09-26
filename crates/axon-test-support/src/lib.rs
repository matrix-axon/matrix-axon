//! Generate test-only signing material in memory; never commit or persist keys.
pub const TEST_KID: &str = "test-ec-1";

pub struct EcTestKey {
    pub pem: String,
    pub x: String,
    pub y: String,
}

pub fn ec_key() -> &'static EcTestKey {
    use aws_lc_rs::signature::KeyPair;
    use base64::Engine;
    static KEY: std::sync::OnceLock<EcTestKey> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        let algorithm = &aws_lc_rs::signature::ECDSA_P256_SHA256_FIXED_SIGNING;
        let der = aws_lc_rs::signature::EcdsaKeyPair::generate_pkcs8(
            algorithm,
            &aws_lc_rs::rand::SystemRandom::new(),
        )
        .unwrap();
        let pair = aws_lc_rs::signature::EcdsaKeyPair::from_pkcs8(algorithm, der.as_ref()).unwrap();
        let public = pair.public_key().as_ref();
        let base64 = &base64::engine::general_purpose::STANDARD;
        let url64 = &base64::engine::general_purpose::URL_SAFE_NO_PAD;
        EcTestKey {
            pem: format!(
                "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
                base64.encode(der.as_ref())
            ),
            x: url64.encode(&public[1..33]),
            y: url64.encode(&public[33..65]),
        }
    })
}
