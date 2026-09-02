use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

/// AES-256-GCM 加密：输出 base64(nonce || ciphertext)
pub fn encrypt(plain: &[u8], master_key: &[u8; 32]) -> Result<String, String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, plain)
        .map_err(|e| format!("encrypt failed: {e}"))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(B64.encode(out))
}

/// 解密 base64(nonce || ciphertext)
pub fn decrypt(enc: &str, master_key: &[u8; 32]) -> Result<String, String> {
    let raw = B64
        .decode(enc)
        .map_err(|e| format!("base64 decode failed: {e}"))?;
    if raw.len() < 12 {
        return Err("ciphertext too short".into());
    }
    let (nonce, ct) = raw.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key));
    let pt = cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|e| format!("decrypt failed: {e}"))?;
    String::from_utf8(pt).map_err(|e| format!("plaintext is not utf8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = [7u8; 32];
        let enc = encrypt(b"sk-abc123", &key).unwrap();
        assert_ne!(enc, "sk-abc123");
        assert_eq!(decrypt(&enc, &key).unwrap(), "sk-abc123");
        // 错误密钥必须解密失败
        let bad = [8u8; 32];
        assert!(decrypt(&enc, &bad).is_err());
    }
}
