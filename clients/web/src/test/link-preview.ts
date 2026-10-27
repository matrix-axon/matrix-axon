/** Representative SDK v2 descriptor; tests never send these fixture keys. */
export const encryptedPreviewFile = {
  url: 'mxc://hs/encrypted-preview',
  v: 'v2',
  key: {
    kty: 'oct',
    alg: 'A256CTR',
    ext: true,
    key_ops: ['encrypt', 'decrypt'],
    k: 'a'.repeat(43),
  },
  iv: 'a'.repeat(22),
  hashes: { sha256: 'a'.repeat(43) },
}
