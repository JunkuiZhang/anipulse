use std::{env, sync::Arc};

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use rand::fill;
use sha2::Sha256;
use tokio::sync::Semaphore;

use crate::{
    config::WebConfig,
    error::{AppError, Result},
    repository::{AuthenticatedSession, Repository, WebAdmin},
};

type HmacSha256 = Hmac<Sha256>;
const MIN_PASSWORD_CHARS: usize = 12;
const MAX_PASSWORD_BYTES: usize = 256;

#[derive(Clone)]
pub struct PasswordService {
    semaphore: Arc<Semaphore>,
}

impl Default for PasswordService {
    fn default() -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(1)),
        }
    }
}

impl PasswordService {
    pub fn validate_password(password: &str) -> Result<()> {
        if password.chars().count() < MIN_PASSWORD_CHARS {
            return Err(AppError::InvalidInput(format!(
                "password must contain at least {MIN_PASSWORD_CHARS} characters"
            )));
        }
        if password.len() > MAX_PASSWORD_BYTES {
            return Err(AppError::InvalidInput(format!(
                "password must not exceed {MAX_PASSWORD_BYTES} bytes"
            )));
        }
        Ok(())
    }

    pub async fn hash(&self, password: String) -> Result<String> {
        Self::validate_password(&password)?;
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AppError::Config("password worker is unavailable".into()))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut salt_bytes = [0_u8; 16];
            fill(&mut salt_bytes);
            let salt = SaltString::encode_b64(&salt_bytes)
                .map_err(|_| AppError::Config("password salt generation failed".into()))?;
            password_argon2()
                .hash_password(password.as_bytes(), &salt)
                .map(|hash| hash.to_string())
                .map_err(|_| AppError::Config("password hashing failed".into()))
        })
        .await
        .map_err(|_| AppError::Config("password worker stopped unexpectedly".into()))?
    }

    pub async fn verify(&self, password: String, encoded_hash: String) -> Result<bool> {
        if password.len() > MAX_PASSWORD_BYTES {
            return Ok(false);
        }
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AppError::Config("password worker is unavailable".into()))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let Ok(hash) = PasswordHash::new(&encoded_hash) else {
                return false;
            };
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        })
        .await
        .map_err(|_| AppError::Config("password worker stopped unexpectedly".into()))
    }
}

fn password_argon2() -> Argon2<'static> {
    let params = Params::new(19 * 1024, 2, 1, None).expect("valid Argon2id parameters");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

#[derive(Clone)]
pub struct AuthKeys {
    session: [u8; 32],
    csrf: [u8; 32],
    audit: [u8; 32],
    action: [u8; 32],
}

impl AuthKeys {
    pub fn from_env() -> Result<Self> {
        let encoded = env::var("ANIPULSE_WEB_SECRET").map_err(|_| {
            AppError::Config("ANIPULSE_WEB_SECRET is required for the web server".into())
        })?;
        let secret = URL_SAFE_NO_PAD
            .decode(encoded.trim())
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(encoded.trim()))
            .map_err(|_| AppError::Config("ANIPULSE_WEB_SECRET must be valid base64".into()))?;
        if secret.len() < 32 {
            return Err(AppError::Config(
                "ANIPULSE_WEB_SECRET must decode to at least 32 bytes".into(),
            ));
        }
        Self::from_secret(&secret)
    }

    fn from_secret(secret: &[u8]) -> Result<Self> {
        let hkdf = Hkdf::<Sha256>::new(Some(b"AniPulse web v1"), secret);
        let mut session = [0_u8; 32];
        let mut csrf = [0_u8; 32];
        let mut audit = [0_u8; 32];
        let mut action = [0_u8; 32];
        hkdf.expand(b"session-token-hmac", &mut session)
            .map_err(|_| AppError::Config("cannot derive session key".into()))?;
        hkdf.expand(b"csrf-token-hmac", &mut csrf)
            .map_err(|_| AppError::Config("cannot derive CSRF key".into()))?;
        hkdf.expand(b"audit-identifier-hmac", &mut audit)
            .map_err(|_| AppError::Config("cannot derive audit key".into()))?;
        hkdf.expand(b"one-time-action-hmac", &mut action)
            .map_err(|_| AppError::Config("cannot derive action key".into()))?;
        Ok(Self {
            session,
            csrf,
            audit,
            action,
        })
    }

    pub fn session_hmac(&self, token: &str) -> Vec<u8> {
        hmac_bytes(&self.session, token.as_bytes())
    }

    pub fn csrf_token(&self, token_hmac: &[u8], scope: &str) -> String {
        let mut value = Vec::with_capacity(token_hmac.len() + scope.len() + 1);
        value.extend_from_slice(token_hmac);
        value.push(0);
        value.extend_from_slice(scope.as_bytes());
        URL_SAFE_NO_PAD.encode(hmac_bytes(&self.csrf, &value))
    }

    pub fn verify_csrf(&self, token_hmac: &[u8], scope: &str, supplied: &str) -> bool {
        let Ok(supplied) = URL_SAFE_NO_PAD.decode(supplied) else {
            return false;
        };
        let mut value = Vec::with_capacity(token_hmac.len() + scope.len() + 1);
        value.extend_from_slice(token_hmac);
        value.push(0);
        value.extend_from_slice(scope.as_bytes());
        HmacSha256::new_from_slice(&self.csrf)
            .expect("HMAC accepts any key length")
            .chain_update(value)
            .verify_slice(&supplied)
            .is_ok()
    }

    pub fn audit_hash(&self, value: &str) -> Vec<u8> {
        hmac_bytes(&self.audit, value.as_bytes())
    }

    fn action_hmac(&self, value: &str) -> Vec<u8> {
        hmac_bytes(&self.action, value.as_bytes())
    }
}

fn hmac_bytes(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(value);
    mac.finalize().into_bytes().to_vec()
}

#[derive(Debug)]
pub enum LoginOutcome {
    Success(NewSession),
    Invalid,
    Blocked(DateTime<Utc>),
    Uninitialized,
}

#[derive(Debug)]
pub struct NewSession {
    pub token: String,
    pub admin_id: i64,
}

#[derive(Debug, Clone)]
pub struct SessionIdentity {
    pub token_hmac: Vec<u8>,
    pub admin_id: i64,
    pub username: String,
    pub role: String,
    pub csrf_token: String,
    pub renewed_token: Option<String>,
}

#[derive(Clone)]
pub struct AuthService {
    repository: Repository,
    config: WebConfig,
    keys: AuthKeys,
    passwords: PasswordService,
    dummy_hash: String,
}

impl AuthService {
    pub async fn from_env(repository: Repository, config: WebConfig) -> Result<Self> {
        let passwords = PasswordService::default();
        let dummy_hash = passwords
            .hash("AniPulse dummy login password".to_string())
            .await?;
        Ok(Self {
            repository,
            config,
            keys: AuthKeys::from_env()?,
            passwords,
            dummy_hash,
        })
    }

    #[cfg(test)]
    pub(crate) async fn for_test(repository: Repository, config: WebConfig) -> Result<Self> {
        let passwords = PasswordService::default();
        let dummy_hash = passwords
            .hash("AniPulse dummy login password".to_string())
            .await?;
        Ok(Self {
            repository,
            config,
            keys: AuthKeys::from_secret(&[0x5a; 32])?,
            passwords,
            dummy_hash,
        })
    }

    pub fn keys(&self) -> &AuthKeys {
        &self.keys
    }

    pub async fn login(
        &self,
        username_input: &str,
        password: String,
        source_ip: &str,
        user_agent: Option<&str>,
    ) -> Result<LoginOutcome> {
        if self.repository.web_admin_count().await? == 0 {
            return Ok(LoginOutcome::Uninitialized);
        }
        let username = normalize_username(username_input).unwrap_or_else(|_| "invalid".into());
        let throttle_keys = vec![
            self.keys.audit_hash(&format!("login-user:{username}")),
            self.keys.audit_hash(&format!("login-ip:{source_ip}")),
        ];
        let now = Utc::now();
        for key in &throttle_keys {
            if let Some(until) = self.repository.auth_throttle_blocked_until(key).await?
                && until > now
            {
                return Ok(LoginOutcome::Blocked(until));
            }
        }

        let admin = self.repository.web_admin_by_username(&username).await?;
        let hash = admin
            .as_ref()
            .map(|admin| admin.password_hash.clone())
            .unwrap_or_else(|| self.dummy_hash.clone());
        let valid = self.passwords.verify(password, hash).await?
            && admin.as_ref().is_some_and(|admin| !admin.disabled);
        if !valid {
            let mut blocked_until = None;
            for key in &throttle_keys {
                blocked_until = blocked_until.max(
                    self.repository
                        .record_auth_failure(
                            key,
                            self.config.login_window_secs,
                            self.config.login_max_failures,
                        )
                        .await?,
                );
            }
            self.repository
                .record_audit(
                    "system",
                    None,
                    "web.login",
                    Some("web_admin"),
                    None,
                    "failure",
                    None,
                    Some(&self.keys.audit_hash(source_ip)),
                    "{}",
                )
                .await?;
            return Ok(blocked_until
                .map(LoginOutcome::Blocked)
                .unwrap_or(LoginOutcome::Invalid));
        }

        let admin = admin.expect("valid login always has an admin");
        self.repository.clear_auth_throttle(&throttle_keys).await?;
        let token = random_token();
        let token_hmac = self.keys.session_hmac(&token);
        self.repository
            .create_web_session(
                &token_hmac,
                admin.id,
                now + chrono::Duration::seconds(self.config.session_idle_secs),
                now + chrono::Duration::seconds(self.config.session_absolute_secs),
                user_agent
                    .map(|value| self.keys.audit_hash(value))
                    .as_deref(),
                Some(&self.keys.audit_hash(source_ip)),
            )
            .await?;
        self.repository
            .record_audit(
                "admin",
                Some(admin.id),
                "web.login",
                Some("web_admin"),
                Some(&admin.id.to_string()),
                "success",
                None,
                Some(&self.keys.audit_hash(source_ip)),
                "{}",
            )
            .await?;
        Ok(LoginOutcome::Success(NewSession {
            token,
            admin_id: admin.id,
        }))
    }

    pub async fn authenticate(&self, token: &str) -> Result<Option<SessionIdentity>> {
        let token_hmac = self.keys.session_hmac(token);
        let Some(session) = self.repository.authenticated_session(&token_hmac).await? else {
            return Ok(None);
        };
        if session.role != "owner" {
            return Ok(None);
        }
        self.refresh_session(token_hmac, session).await.map(Some)
    }

    async fn refresh_session(
        &self,
        token_hmac: Vec<u8>,
        session: AuthenticatedSession,
    ) -> Result<SessionIdentity> {
        let now = Utc::now();
        let idle_expiry = (now + chrono::Duration::seconds(self.config.session_idle_secs))
            .min(session.absolute_expires_at);
        let should_renew =
            session.renewed_at + chrono::Duration::seconds(self.config.session_renewal_secs) <= now;
        let (active_hmac, renewed_token) = if should_renew {
            let new_token = random_token();
            let new_hmac = self.keys.session_hmac(&new_token);
            self.repository
                .rotate_web_session(&token_hmac, &new_hmac, idle_expiry)
                .await?;
            (new_hmac, Some(new_token))
        } else {
            self.repository
                .touch_web_session(&token_hmac, idle_expiry)
                .await?;
            (token_hmac, None)
        };
        Ok(SessionIdentity {
            csrf_token: self.keys.csrf_token(&active_hmac, "form"),
            token_hmac: active_hmac,
            admin_id: session.admin_id,
            username: session.username,
            role: session.role,
            renewed_token,
        })
    }

    pub async fn logout(&self, identity: &SessionIdentity) -> Result<()> {
        self.repository
            .revoke_web_session(&identity.token_hmac)
            .await
    }

    pub fn verify_form_csrf(&self, identity: &SessionIdentity, token: &str) -> bool {
        self.keys.verify_csrf(&identity.token_hmac, "form", token)
    }

    pub async fn issue_action_nonce(
        &self,
        identity: &SessionIdentity,
        action: &str,
        entity_id: Option<&str>,
    ) -> Result<String> {
        let nonce = random_token();
        self.repository
            .create_action_nonce(
                &self.keys.action_hmac(&nonce),
                identity.admin_id,
                action,
                entity_id,
            )
            .await?;
        Ok(nonce)
    }

    pub async fn consume_action_nonce(
        &self,
        identity: &SessionIdentity,
        nonce: &str,
        action: &str,
        entity_id: Option<&str>,
    ) -> Result<bool> {
        self.repository
            .consume_action_nonce(
                &self.keys.action_hmac(nonce),
                identity.admin_id,
                action,
                entity_id,
            )
            .await
    }
}

pub fn normalize_username(value: &str) -> Result<String> {
    let username = value.trim().to_ascii_lowercase();
    if !(3..=32).contains(&username.len())
        || !username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        || !username
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(AppError::InvalidInput(
            "username must be 3-32 ASCII letters, digits, '.', '_' or '-' and start with a letter or digit"
                .into(),
        ));
    }
    Ok(username)
}

pub async fn create_admin(
    repository: &Repository,
    username: &str,
    password: String,
) -> Result<i64> {
    let username = normalize_username(username)?;
    if repository.web_admin_count().await? != 0 {
        return Err(AppError::InvalidInput(
            "an owner administrator already exists".into(),
        ));
    }
    let hash = PasswordService::default().hash(password).await?;
    repository.create_web_admin(&username, &hash).await
}

pub async fn reset_admin_password(
    repository: &Repository,
    username: &str,
    password: String,
) -> Result<()> {
    let username = normalize_username(username)?;
    let hash = PasswordService::default().hash(password).await?;
    repository.reset_web_admin_password(&username, &hash).await
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn admin_is_enabled(admin: &WebAdmin) -> bool {
    !admin.disabled && admin.role == "owner"
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn username_normalization_is_strict() {
        assert_eq!(normalize_username(" Admin.User ").unwrap(), "admin.user");
        assert!(normalize_username("ab").is_err());
        assert!(normalize_username("管理员").is_err());
        assert!(normalize_username("-admin").is_err());
    }

    #[test]
    fn csrf_tokens_are_session_and_scope_bound() {
        let keys = AuthKeys::from_secret(&[7; 32]).unwrap();
        let first_session = keys.session_hmac("first");
        let second_session = keys.session_hmac("second");
        let token = keys.csrf_token(&first_session, "form");
        assert!(keys.verify_csrf(&first_session, "form", &token));
        assert!(!keys.verify_csrf(&second_session, "form", &token));
        assert!(!keys.verify_csrf(&first_session, "json", &token));
        assert!(!keys.verify_csrf(&first_session, "form", "not-base64!"));
    }

    #[test]
    fn opaque_tokens_have_256_bits_of_random_input() {
        let first = random_token();
        let second = random_token();
        assert_eq!(URL_SAFE_NO_PAD.decode(&first).unwrap().len(), 32);
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn password_hashes_and_verifies_with_argon2id() {
        let service = PasswordService::default();
        let hash = service
            .hash("a reasonably long password".into())
            .await
            .unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(
            service
                .verify("a reasonably long password".into(), hash.clone())
                .await
                .unwrap()
        );
        assert!(!service.verify("wrong password".into(), hash).await.unwrap());
    }

    #[tokio::test]
    async fn password_reset_revokes_sessions() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("auth.db");
        let repository = Repository::connect(path.to_str().unwrap()).await.unwrap();
        let admin_id = create_admin(&repository, "admin", "initial password 123".into())
            .await
            .unwrap();
        let now = Utc::now();
        repository
            .create_web_session(
                b"old-session-hmac",
                admin_id,
                now + chrono::Duration::hours(1),
                now + chrono::Duration::hours(2),
                None,
                None,
            )
            .await
            .unwrap();
        reset_admin_password(&repository, "admin", "replacement password 456".into())
            .await
            .unwrap();
        assert!(
            repository
                .authenticated_session(b"old-session-hmac")
                .await
                .unwrap()
                .is_none()
        );
    }
}
