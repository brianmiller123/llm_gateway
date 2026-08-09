//! API Key 明文的可逆存储：AES-256-GCM（密钥 = 主密钥），nonce(12B) || ciphertext，base64。
//! 仅用于"创建后可再次复制"；鉴权仍走 SHA-256 哈希。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;

/// 加密明文 Key
pub fn encrypt(master: &[u8; 32], plain: &str) -> Result<String, String> {
    let cipher = Aes256Gcm::new_from_slice(master).map_err(|_| "invalid master key".to_string())?;
    let mut nonce = [0u8; 12];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_bytes())
        .map_err(|_| "encryption failed".to_string())?;
    let mut out = Vec::with_capacity(nonce.len() + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(base64::engine::general_purpose::STANDARD.encode(out))
}

/// 解密（nonce 前置）
pub fn decrypt(master: &[u8; 32], b64: &str) -> Result<String, String> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| "invalid encrypted key blob".to_string())?;
    if raw.len() < 13 {
        return Err("invalid encrypted key blob".into());
    }
    let (nonce, ct) = raw.split_at(12);
    let cipher = Aes256Gcm::new_from_slice(master).map_err(|_| "invalid master key".to_string())?;
    let pt = cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| "decryption failed (master key changed?)".to_string())?;
    String::from_utf8(pt).map_err(|_| "decrypted key is not utf8".to_string())
}
