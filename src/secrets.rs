// 密钥与敏感字段加解密。
//
// 密钥文件 /data/secret.key（0600，32 字节随机）；拿不到密钥就启动失败，
// 不做“静默生成新密钥”的兜底——否则旧密文会被解成乱码。
// 密文格式：v1:<nonce_b64>:<ciphertext_b64>

use std::path::Path;

use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead, KeyInit},
};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const VERSION: &str = "v1";

/// 持有主密钥的加解密器。
pub struct Sealer {
    cipher: ChaCha20Poly1305,
}

impl Sealer {
    /// 从文件加载密钥。文件不存在时按需生成（仅首次启动）。
    ///
    /// 权限：文件必须以 0600 保存；发现权限过于宽松只告警，不改动。
    pub fn load_or_create(path: &Path) -> Result<Sealer, String> {
        if path.exists() {
            let raw = std::fs::read(path)
                .map_err(|e| format!("读取密钥文件 {} 失败: {e}", path.display()))?;
            let key = normalize_key(&raw, path)?;
            return Ok(Sealer::from_key(key));
        }

        // 首次启动：生成 32 字节随机密钥。
        let mut key = [0u8; KEY_LEN];
        rand::fill(&mut key);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("创建数据目录 {} 失败: {e}", dir.display()))?;
        }
        std::fs::write(path, hex::encode(key))
            .map_err(|e| format!("写入密钥文件 {} 失败: {e}", path.display()))?;
        restrict_permissions(path)?;
        Ok(Sealer::from_key(key))
    }

    /// 用给定密钥构造（测试用）。
    pub fn from_key(key: [u8; KEY_LEN]) -> Sealer {
        Sealer { cipher: ChaCha20Poly1305::new(&key.into()) }
    }

    /// 加密一段明文，返回可直接入库的字符串。
    pub fn seal(&self, plaintext: &str) -> Result<String, String> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce);
        let nonce_ga = Nonce::from(nonce);
        let ct = self
            .cipher
            .encrypt(&nonce_ga, plaintext.as_bytes())
            .map_err(|_| "加密失败".to_string())?;
        Ok(format!("{VERSION}:{}:{}", hex::encode(nonce), hex::encode(&ct)))
    }

    /// 解密。密钥不对或数据被动过都会返回错误。
    pub fn open(&self, sealed: &str) -> Result<String, String> {
        let mut parts = sealed.splitn(3, ':');
        let (Some(ver), Some(nonce_b64), Some(ct_b64)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err("密文格式不正确".to_string());
        };
        if ver != VERSION {
            return Err(format!("不支持的密文版本: {ver}"));
        }
        let nonce_raw = hex::decode(nonce_b64).map_err(|e| format!("密文 nonce 不是十六进制: {e}"))?;
        if nonce_raw.len() != NONCE_LEN {
            return Err("密文 nonce 长度不正确".to_string());
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&nonce_raw);
        let ct = hex::decode(ct_b64).map_err(|e| format!("密文不是十六进制: {e}"))?;
        let pt = self
            .cipher
            .decrypt(&Nonce::from(nonce), ct.as_ref())
            .map_err(|_| "解密失败（密钥不匹配或数据损坏）".to_string())?;
        String::from_utf8(pt).map_err(|_| "解密结果不是有效 UTF-8".to_string())
    }
}

fn normalize_key(raw: &[u8], path: &Path) -> Result<[u8; KEY_LEN], String> {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim();
    let bytes = if text.len() == KEY_LEN * 2 && text.chars().all(|c| c.is_ascii_hexdigit()) {
        hex::decode(text).map_err(|e| format!("密钥文件 {} 不是十六进制: {e}", path.display()))?
    } else {
        raw.to_vec()
    };
    if bytes.len() != KEY_LEN {
        return Err(format!(
            "密钥文件 {} 长度不对：期望 {KEY_LEN} 字节，实际 {} 字节",
            path.display(),
            bytes.len()
        ));
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(key)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("设置密钥文件权限失败: {e}"))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 加解密往返() {
        let sealer = Sealer::from_key([7u8; KEY_LEN]);
        let sealed = sealer.seal("114514- token").unwrap();
        assert!(sealed.starts_with("v1:"));
        assert_ne!(sealed, "114514- token");
        assert_eq!(sealer.open(&sealed).unwrap(), "114514- token");
    }

    #[test]
    fn 换密钥解不开() {
        let a = Sealer::from_key([1u8; KEY_LEN]);
        let b = Sealer::from_key([2u8; KEY_LEN]);
        let sealed = a.seal("secret").unwrap();
        assert!(b.open(&sealed).is_err());
    }

    #[test]
    fn 密文被篡改要报错() {
        let sealer = Sealer::from_key([3u8; KEY_LEN]);
        let sealed = sealer.seal("secret").unwrap();
        let mut bytes = sealed.into_bytes();
        let last = bytes.len() - 1;
        bytes[last] = if bytes[last] == b'a' { b'b' } else { b'a' };
        let tampered = String::from_utf8(bytes).unwrap();
        assert!(sealer.open(&tampered).is_err());
    }
}
