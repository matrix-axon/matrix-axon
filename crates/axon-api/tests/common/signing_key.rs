/// Throwaway P-256 test keypair (generated once for this fixture via
/// `openssl ecparam -genkey -name prime256v1 | openssl pkcs8 -topk8
/// -nocrypt`, the PKCS#8 form `jsonwebtoken::EncodingKey::from_ec_pem`
/// requires). Not a real provider's key.
const TEST_EC_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgeOm6i9QpS7o68eCA
NG8y2bi5Db3qRTpxAxVgeBqN+i6hRANCAARt+sT4b0RI4+EleJFq3v0AvYszoUuY
oB9x0i2BP6shJia8ab4bDmIToTYbb7isvFYnHGqiCrNlFNHGC8zlOr8n
-----END PRIVATE KEY-----
";
/// The same keypair's public coordinates (base64url, no padding) — what a
/// real JWKS document would publish.
const TEST_EC_X: &str = "bfrE-G9ESOPhJXiRat79AL2LM6FLmKAfcdItgT-rISY";
const TEST_EC_Y: &str = "JrxpvhsOYhOhNhtvuKy8ViccaqIKs2UU0cYLzOU6vyc";
const TEST_KID: &str = "test-ec-1";
