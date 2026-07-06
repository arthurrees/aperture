//! Software WebAuthn authenticator - assertion (`navigator.credentials.get`) only.
//!
//! WebView2 presents host-embedded webviews to extensions as detached popup windows, which breaks
//! the Bitwarden extension's passkey popout. So Aperture performs the FIDO2 assertion itself: it
//! reads the P-256 passkey from the user's Bitwarden vault and signs the relying party's challenge.
//! This is exactly the operation the extension would perform - the site verifies the signature
//! against the public key it stored at registration, so it can't tell the difference.
//!
//! SECURITY: the caller MUST pass the REAL page origin (derived from the tab URL, never page-
//! supplied) and MUST reject any rpId that is not a registrable suffix of that origin's host, so a
//! hostile page can never phish an assertion for another site.

use base64::Engine;
use sha2::{Digest, Sha256};

/// A passkey pulled from the vault, ready to sign with.
pub struct Passkey {
    /// As stored by Bitwarden - usually a GUID string (36 chars), occasionally base64.
    pub credential_id: String,
    /// base64(url or std) of the user handle bytes.
    pub user_handle: String,
    /// The PKCS#8 DER private key bytes.
    pub private_key_pkcs8: Vec<u8>,
    pub rp_id: String,
    pub counter: u32,
}

/// All fields base64url (no pad) for the JS side to rebuild a PublicKeyCredential.
pub struct Assertion {
    pub credential_id_b64u: String,
    pub authenticator_data_b64u: String,
    pub signature_b64u: String,
    pub user_handle_b64u: String,
    pub client_data_json_b64u: String,
}

fn b64u(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode base64 in whatever common flavor it arrives in (url/standard, padded/not).
pub fn b64_any_decode(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    URL_SAFE_NO_PAD
        .decode(s)
        .or_else(|_| URL_SAFE.decode(s))
        .or_else(|_| STANDARD_NO_PAD.decode(s))
        .or_else(|_| STANDARD.decode(s))
        .ok()
}

/// Bitwarden stores the credential id as a GUID string; the rawId the RP knows is its 16 bytes (in
/// order, no .NET mixed-endian swap). Fall back to base64 for non-GUID forms.
fn credential_id_bytes(cred: &str) -> Vec<u8> {
    let hex: String = cred.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cred.contains('-') && hex.len() == 32 {
        (0..16)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0))
            .collect()
    } else {
        b64_any_decode(cred).unwrap_or_default()
    }
}

/// True if `rp_id` is the origin host or a parent domain of it (the rule a real authenticator
/// enforces: rpId must be a registrable suffix of the caller origin's effective domain).
pub fn rp_id_allowed(rp_id: &str, origin_host: &str) -> bool {
    let rp = rp_id.trim_end_matches('.').to_lowercase();
    let host = origin_host.trim_end_matches('.').to_lowercase();
    if host != rp && !host.ends_with(&format!(".{rp}")) {
        return false;
    }
    psl::suffix_str(&rp).is_some_and(|suffix| suffix != rp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rp_id_must_match_host_or_parent_registrable_domain() {
        assert!(rp_id_allowed("example.com", "login.example.com"));
        assert!(rp_id_allowed("login.example.com", "login.example.com"));
        assert!(!rp_id_allowed("evil-example.com", "login.example.com"));
        assert!(!rp_id_allowed("com", "login.example.com"));
        assert!(!rp_id_allowed("co.uk", "school.co.uk"));
    }
}

/// Build and sign a WebAuthn assertion for `get()`.
/// `challenge_b64u` is the RP challenge as the page provided it; `origin` is the real page origin
/// (e.g. `https://app.tenant.example.com`).
pub fn assert(pk: &Passkey, challenge_b64u: &str, origin: &str) -> Result<Assertion, String> {
    use p256::ecdsa::{signature::Signer, Signature, SigningKey};
    use p256::pkcs8::DecodePrivateKey;

    // clientDataJSON. The challenge stays base64url; origin is the host-verified page origin.
    let client_data = format!(
        "{{\"type\":\"webauthn.get\",\"challenge\":\"{challenge_b64u}\",\"origin\":\"{origin}\",\"crossOrigin\":false}}"
    );
    let client_data_hash = Sha256::digest(client_data.as_bytes());

    // authenticatorData = SHA256(rpId)[32] || flags[1] || signCount[4]
    let rp_id_hash = Sha256::digest(pk.rp_id.as_bytes());
    let mut auth_data = Vec::with_capacity(37);
    auth_data.extend_from_slice(&rp_id_hash);
    auth_data.push(0x05); // UP (0x01) | UV (0x04)
    auth_data.extend_from_slice(&pk.counter.to_be_bytes());

    // ECDSA-P256 over (authenticatorData || clientDataHash); the signer hashes with SHA-256. DER out.
    let signing_key = SigningKey::from_pkcs8_der(&pk.private_key_pkcs8)
        .map_err(|e| format!("bad passkey private key: {e}"))?;
    let mut signed = auth_data.clone();
    signed.extend_from_slice(&client_data_hash);
    let sig: Signature = signing_key.sign(&signed);
    let sig_der = sig.to_der();

    Ok(Assertion {
        credential_id_b64u: b64u(&credential_id_bytes(&pk.credential_id)),
        authenticator_data_b64u: b64u(&auth_data),
        signature_b64u: b64u(sig_der.as_bytes()),
        user_handle_b64u: b64u(&b64_any_decode(&pk.user_handle).unwrap_or_default()),
        client_data_json_b64u: b64u(client_data.as_bytes()),
    })
}
