// 密码哈希与会话工具。

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;

/// 生成 argon2 哈希。
pub fn hash_password(password: &str) -> Result<String, String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| format!("生成密码哈希失败: {e}"))
}

/// 校验密码。
pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 密码哈希可校验() {
        let hash = hash_password("s3cret-pass").unwrap();
        assert!(hash.starts_with("$argon2"));
        assert!(verify_password("s3cret-pass", &hash));
        assert!(!verify_password("wrong", &hash));
        assert!(!verify_password("s3cret-pass", "not-a-hash"));
    }

    #[test]
    fn 同一密码两次哈希不同() {
        let a = hash_password("x").unwrap();
        let b = hash_password("x").unwrap();
        assert_ne!(a, b);
        assert!(verify_password("x", &a) && verify_password("x", &b));
    }
}
