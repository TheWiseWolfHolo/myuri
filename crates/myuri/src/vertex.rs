//! Signing in to Google Vertex AI. A Vertex provider keeps a service account
//! (its e-mail address and private key, as JSON) where other providers keep
//! an API key. The account signs a short statement, Google trades it for an
//! access token good for an hour, and requests carry that token.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer};
use serde_json::{Value, json};

use crate::net;

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Tokens last an hour; a new one is fetched a little before that.
const KEEP: Duration = Duration::from_secs(50 * 60);

/// The access token for a provider, from memory while it is fresh. Blocks on
/// the network when a new one is needed.
pub fn token(provider: &str, account: &str) -> Result<String, String> {
    static TOKENS: Mutex<Option<HashMap<String, (String, Instant)>>> = Mutex::new(None);
    let cached = |tokens: &Option<HashMap<String, (String, Instant)>>| {
        let (token, since) = tokens.as_ref()?.get(provider)?;
        (since.elapsed() < KEEP).then(|| token.clone())
    };
    if let Some(token) = cached(&TOKENS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())) {
        return Ok(token);
    }
    let token = sign_in(account)?;
    let mut tokens = TOKENS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    tokens.get_or_insert_with(HashMap::new).insert(provider.to_string(), (token.clone(), Instant::now()));
    Ok(token)
}

fn sign_in(account: &str) -> Result<String, String> {
    let account: Value =
        serde_json::from_str(account).map_err(|_| "Vertex 渠道的密钥要填服务账号的 JSON".to_string())?;
    let (Some(email), Some(key)) = (account["client_email"].as_str(), account["private_key"].as_str()) else {
        return Err("服务账号 JSON 里缺 client_email 或 private_key".into());
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|now| now.as_secs()).unwrap_or(0);
    let encode = |value: Value| URL_SAFE_NO_PAD.encode(value.to_string());
    let statement = format!(
        "{}.{}",
        encode(json!({ "alg": "RS256", "typ": "JWT" })),
        encode(json!({
            "iss": email,
            "scope": "https://www.googleapis.com/auth/cloud-platform",
            "aud": TOKEN_URL,
            "iat": now,
            "exp": now + 3600,
        }))
    );
    let key = RsaPrivateKey::from_pkcs8_pem(key).map_err(|_| "服务账号的私钥读不出来".to_string())?;
    let signature = SigningKey::<Sha256>::new(key).sign(statement.as_bytes());
    let assertion = format!("{statement}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()));
    let form = format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={assertion}");
    let headers = [("content-type".to_string(), "application/x-www-form-urlencoded".to_string())];
    let (status, body) = net::post(TOKEN_URL, &headers, form.as_bytes())?;
    let reply: Value = serde_json::from_slice(&body).unwrap_or_default();
    match reply["access_token"].as_str() {
        Some(token) if (200..300).contains(&status) => Ok(token.to_string()),
        _ => {
            let reason = reply["error_description"].as_str().or(reply["error"].as_str()).unwrap_or("没有说明原因");
            Err(format!("Google 不接受这个服务账号（HTTP {status}）：{reason}"))
        }
    }
}

/// Replace the API key header of a Gemini request with the access token.
pub fn authorize(headers: &mut Vec<(String, String)>, token: &str) {
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("x-goog-api-key"));
    headers.push(("authorization".into(), format!("Bearer {token}")));
}
