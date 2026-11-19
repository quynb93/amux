//! Web Push: notifications straight to the phone app (`amux serve`), with no other app on
//! the phone. The app subscribes through the browser's push service (Apple, Google,
//! Mozilla…) and hands amux the subscription; amux encrypts each message to it
//! (RFC 8291, `aes128gcm` from RFC 8188) and signs the request with its own key (VAPID,
//! RFC 8292). The push service sees when a message goes, never what it says.
//!
//! Kept in ~/.amux, both mode 0600:
//!   webpush-key.json   the VAPID key pair (P-256), made on first use
//!   webpush-subs.json  the subscribed devices: endpoint, p256dh and auth each

use crate::store::{read_json, write_json};
use crate::{paths, time};
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use ring::{aead, agreement, hkdf};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Who runs this application server, as RFC 8292 asks: the project, not the user.
const CONTACT: &str = "https://github.com/quynb93/amux";
/// How long a push service keeps a message for a phone that's offline.
const TTL_SECS: u64 = 12 * 3600;
/// Record size in the aes128gcm header. One record holds the whole message.
const RECORD_SIZE: u32 = 4096;
/// Push services take at least 4096 bytes of body; keep well under it.
const MAX_BODY_CHARS: usize = 1000;
const MAX_DEVICES: usize = 20;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Sub {
    /// The push service's URL for this one browser.
    pub endpoint: String,
    /// The browser's P-256 public key (uncompressed point), base64url.
    pub p256dh: String,
    /// The 16-byte authentication secret, base64url.
    pub auth: String,
    /// What kind of device it is ("iPhone", "Android"…), for `amux phone`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device: String,
    #[serde(default)]
    pub added: i64,
}

#[derive(Serialize, Deserialize, Default)]
struct Subs {
    #[serde(default)]
    subscriptions: Vec<Sub>,
}

#[derive(Serialize, Deserialize, Default)]
struct KeyFile {
    /// The private key as PKCS#8, base64url.
    #[serde(default)]
    pkcs8: String,
}

fn key_file() -> PathBuf {
    paths::AMUX_HOME.join("webpush-key.json")
}

pub fn subs_file() -> PathBuf {
    paths::AMUX_HOME.join("webpush-subs.json")
}

/// base64url, with or without padding, and tolerant of the standard alphabet.
fn unb64(s: &str) -> Result<Vec<u8>> {
    let s: String = s
        .trim()
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    B64.decode(s).map_err(|e| anyhow!("bad base64: {e}"))
}

/// The server's VAPID key pair, made (and saved) the first time it's needed.
fn key_at(file: &Path) -> Result<EcdsaKeyPair> {
    let rng = SystemRandom::new();
    let load = |f: &Path| -> Result<Option<EcdsaKeyPair>> {
        let k: KeyFile = read_json(f)?;
        if k.pkcs8.is_empty() {
            return Ok(None);
        }
        let der = unb64(&k.pkcs8)?;
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &rng)
            .map(Some)
            .map_err(|e| {
                anyhow!(
                    "{} holds a bad key ({e}); delete it to make a new one",
                    f.display()
                )
            })
    };
    if let Some(k) = load(file)? {
        return Ok(k);
    }
    let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
        .map_err(|_| anyhow!("could not make a key"))?;
    let text = serde_json::to_string_pretty(&KeyFile {
        pkcs8: B64.encode(doc.as_ref()),
    })? + "\n";
    if let Some(dir) = file.parent() {
        crate::fsx::mkdir_private(dir)?;
    }
    // Written aside, then linked into place only if no key is there yet: two processes
    // making a key at once agree on one, and nobody reads half a file.
    let tmp = file.with_extension(format!("{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    crate::fsx::write_atomic(&tmp, text.as_bytes())?;
    let linked = std::fs::hard_link(&tmp, file);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("could not write {}", file.display())),
    }
    load(file)?.context("the key file is empty")
}

fn key() -> Result<EcdsaKeyPair> {
    key_at(&key_file())
}

/// The server's public key (base64url), which the app subscribes with.
pub fn public_key() -> Result<String> {
    Ok(B64.encode(key()?.public_key().as_ref()))
}

fn load_at(file: &Path) -> Vec<Sub> {
    read_json::<Subs>(file).unwrap_or_default().subscriptions
}

pub fn load_subs() -> Vec<Sub> {
    load_at(&subs_file())
}

fn save_at(file: &Path, subs: Vec<Sub>) -> Result<()> {
    write_json(
        file,
        &Subs {
            subscriptions: subs,
        },
    )
}

/// Checks a subscription from the app: a push service's URL (HTTPS; plain HTTP only on
/// this machine, for tests), a P-256 point and a 16-byte secret.
pub fn validate(endpoint: &str, p256dh: &str, auth: &str, device: &str) -> Result<Sub> {
    if endpoint.len() > 2048 {
        bail!("the endpoint is too long");
    }
    let u = url::Url::parse(endpoint).map_err(|_| anyhow!("the endpoint isn't a URL"))?;
    let local = matches!(
        u.host_str(),
        Some("127.0.0.1") | Some("localhost") | Some("[::1]")
    );
    if !(u.scheme() == "https" || (u.scheme() == "http" && local)) || u.host_str().is_none() {
        bail!("the endpoint should be an https URL");
    }
    let key = unb64(p256dh).context("p256dh")?;
    if key.len() != 65 || key[0] != 4 {
        bail!("p256dh should be an uncompressed P-256 point");
    }
    if unb64(auth).context("auth")?.len() != 16 {
        bail!("auth should be 16 bytes");
    }
    let device: String = device
        .chars()
        .filter(|c| c.is_alphanumeric() || " -_.".contains(*c))
        .take(40)
        .collect();
    Ok(Sub {
        endpoint: endpoint.to_string(),
        p256dh: B64.encode(&key),
        auth: B64.encode(unb64(auth)?),
        device,
        added: time::now_ms(),
    })
}

/// Adds (or refreshes) a device; returns how many there are. The oldest go past 20.
fn add_at(file: &Path, sub: Sub) -> Result<usize> {
    let mut subs = load_at(file);
    subs.retain(|s| s.endpoint != sub.endpoint);
    subs.push(sub);
    if subs.len() > MAX_DEVICES {
        let extra = subs.len() - MAX_DEVICES;
        subs.drain(..extra);
    }
    let n = subs.len();
    save_at(file, subs)?;
    Ok(n)
}

pub fn add(sub: Sub) -> Result<usize> {
    add_at(&subs_file(), sub)
}

/// Removes the devices with these endpoints; returns how many went.
fn remove_at(file: &Path, endpoints: &[&str]) -> Result<usize> {
    let mut subs = load_at(file);
    let before = subs.len();
    subs.retain(|s| !endpoints.contains(&s.endpoint.as_str()));
    let gone = before - subs.len();
    if gone > 0 {
        save_at(file, subs)?;
    }
    Ok(gone)
}

pub fn remove(endpoint: &str) -> Result<usize> {
    remove_at(&subs_file(), &[endpoint])
}

struct Len(usize);
impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> hkdf::Prk {
    hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm)
}

fn hkdf_expand(prk: &hkdf::Prk, info: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    prk.expand(&[info], Len(len))
        .and_then(|okm| okm.fill(&mut out))
        .expect("HKDF output length is within limits");
    out
}

/// The content key and nonce for a message (RFC 8291 section 3.4).
fn derive(
    ecdh_secret: &[u8],
    auth: &[u8],
    salt: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
) -> (Vec<u8>, Vec<u8>) {
    let prk_key = hkdf_extract(auth, ecdh_secret);
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let ikm = hkdf_expand(&prk_key, &key_info, 32);
    let prk = hkdf_extract(salt, &ikm);
    let cek = hkdf_expand(&prk, b"Content-Encoding: aes128gcm\0", 16);
    let nonce = hkdf_expand(&prk, b"Content-Encoding: nonce\0", 12);
    (cek, nonce)
}

/// The aes128gcm body for `plaintext`, given the ECDH secret and everything else that
/// goes into it: salt, record size, key id (the sender's public key), then one record
/// ending with the 0x02 delimiter.
fn encrypt_with(
    ecdh_secret: &[u8],
    auth: &[u8],
    salt: &[u8; 16],
    ua_public: &[u8],
    as_public: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let (cek, nonce) = derive(ecdh_secret, auth, salt, ua_public, as_public);
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| anyhow!("bad key"))?,
    );
    let nonce = aead::Nonce::try_assume_unique_for_key(&nonce).map_err(|_| anyhow!("bad nonce"))?;
    let mut record = plaintext.to_vec();
    record.push(2);
    key.seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut record)
        .map_err(|_| anyhow!("could not encrypt"))?;
    let mut out = salt.to_vec();
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(as_public);
    out.extend_from_slice(&record);
    Ok(out)
}

/// Encrypts `plaintext` to a subscription with a fresh key pair and salt.
fn encrypt(ua_public: &[u8], auth: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let rng = SystemRandom::new();
    let mut salt = [0u8; 16];
    rng.fill(&mut salt).map_err(|_| anyhow!("no randomness"))?;
    let mine = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| anyhow!("could not make a key"))?;
    let as_public = mine
        .compute_public_key()
        .map_err(|_| anyhow!("could not make a key"))?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public);
    let secret = agreement::agree_ephemeral(mine, &peer, |s| s.to_vec())
        .map_err(|_| anyhow!("the subscription's key isn't a valid P-256 point"))?;
    encrypt_with(
        &secret,
        auth,
        &salt,
        ua_public,
        as_public.as_ref(),
        plaintext,
    )
}

/// The VAPID token (an ES256 JWT) for a push service at `aud` (its origin).
fn vapid_jwt(key: &EcdsaKeyPair, aud: &str, exp: u64) -> Result<String> {
    let header = B64.encode(r#"{"typ":"JWT","alg":"ES256"}"#);
    let claims =
        B64.encode(serde_json::json!({ "aud": aud, "exp": exp, "sub": CONTACT }).to_string());
    let signing = format!("{header}.{claims}");
    let sig = key
        .sign(&SystemRandom::new(), signing.as_bytes())
        .map_err(|_| anyhow!("could not sign"))?;
    Ok(format!("{signing}.{}", B64.encode(sig.as_ref())))
}

/// The Authorization header for a request to `endpoint` (RFC 8292): a token for its
/// origin that expires in 12 hours, and the public key.
fn vapid_header(key: &EcdsaKeyPair, endpoint: &str) -> Result<String> {
    let u = url::Url::parse(endpoint)?;
    let aud = u.origin().ascii_serialization();
    let exp = (time::now_ms().max(0) as u64) / 1000 + 12 * 3600;
    let jwt = vapid_jwt(key, &aud, exp)?;
    Ok(format!(
        "vapid t={jwt}, k={}",
        B64.encode(key.public_key().as_ref())
    ))
}

/// What the service worker gets: the title, the text and the link a tap opens.
pub fn payload(run: &str, title: &str, body: &str, url: Option<&str>) -> String {
    let body: String = if body.chars().count() > MAX_BODY_CHARS {
        body.chars().take(MAX_BODY_CHARS - 1).collect::<String>() + "…"
    } else {
        body.to_string()
    };
    serde_json::json!({ "title": title, "body": body, "url": url, "tag": run }).to_string()
}

enum Sent {
    Ok,
    /// The push service says the subscription is gone (404/410).
    Gone,
}

fn post(
    agent: &ureq::Agent,
    key: &EcdsaKeyPair,
    sub: &Sub,
    body: &[u8],
    urgent: bool,
) -> Result<Sent> {
    let ua_public = unb64(&sub.p256dh)?;
    let auth = unb64(&sub.auth)?;
    let data = encrypt(&ua_public, &auth, body)?;
    let res = agent
        .post(&sub.endpoint)
        .set("TTL", &TTL_SECS.to_string())
        .set("Urgency", if urgent { "high" } else { "normal" })
        .set("Content-Encoding", "aes128gcm")
        .set("Content-Type", "application/octet-stream")
        .set("Authorization", &vapid_header(key, &sub.endpoint)?)
        .send_bytes(&data);
    match res {
        Ok(_) => Ok(Sent::Ok),
        Err(ureq::Error::Status(404 | 410, _)) => Ok(Sent::Gone),
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            bail!("{code} {}", crate::util::one_line(&text, 200))
        }
        Err(e) => Err(e.into()),
    }
}

/// Sends one notification to every subscribed device and waits for the answers. Devices
/// the push service has forgotten are removed.
pub fn deliver(
    agent: &ureq::Agent,
    run: &str,
    title: &str,
    message: &str,
    link: Option<&str>,
    urgent: bool,
) -> Result<()> {
    let subs = load_subs();
    if subs.is_empty() {
        bail!(
            "no device has turned on notifications yet: open the phone app (amux serve, over HTTPS), Monitor → Notifications"
        );
    }
    let key = key()?;
    let body = payload(run, title, message, link);
    let (mut sent, mut gone, mut errors) = (0, vec![], vec![]);
    for s in &subs {
        match post(agent, &key, s, body.as_bytes(), urgent) {
            Ok(Sent::Ok) => sent += 1,
            Ok(Sent::Gone) => gone.push(s.endpoint.as_str()),
            Err(e) => {
                let host = url::Url::parse(&s.endpoint)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                    .unwrap_or_default();
                errors.push(format!("{host}: {e:#}"));
            }
        }
    }
    if !gone.is_empty() {
        let _ = remove_at(&subs_file(), &gone);
    }
    if !errors.is_empty() {
        bail!(
            "sent to {sent} of {} devices; {}",
            subs.len(),
            errors.join("; ")
        );
    }
    if sent == 0 {
        bail!("the push service no longer knows this device (it was removed): turn notifications on again in the app");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn b(s: &str) -> Vec<u8> {
        unb64(&s.split_whitespace().collect::<String>()).unwrap()
    }

    /// RFC 8291, section 5 and Appendix A.
    #[test]
    fn encrypts_the_rfc_8291_example_exactly() {
        let plaintext = b("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");
        assert_eq!(plaintext, b"When I grow up, I want to be a watermelon");
        let as_public = b("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIg
                           Dll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
        let ua_public = b("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx
                           aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        let salt: [u8; 16] = b("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let auth = b("BTBZMqHH6r4Tts7J_aSIgg");
        let ecdh_secret = b("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs");

        // The intermediate values.
        let prk_key = hkdf_extract(&auth, &ecdh_secret);
        let mut key_info = b"WebPush: info\0".to_vec();
        key_info.extend_from_slice(&ua_public);
        key_info.extend_from_slice(&as_public);
        assert_eq!(
            key_info,
            b("V2ViUHVzaDogaW5mbwAEJXGyvs3942BVG q8e0PTNNmwR zr5VX4m8t7GGpTM5FzFo7OLr4BhZe9MEebhuPI-OztV3
               ylkYfpJGmQ22ggCLDgT-M_SrDepxkU21WCP3O1SUj0Ew
               bZIHMtu5pZpTKGSCIA5Zent7wmC6HCJ5mFgJkuk5cwAv MBKiiujwa7t45ewP")
        );
        let ikm = hkdf_expand(&prk_key, &key_info, 32);
        assert_eq!(ikm, b("S4lYMb_L0FxCeq0WhDx813KgSYqU26kOyzWUdsXYyrg"));
        let (cek, nonce) = derive(&ecdh_secret, &auth, &salt, &ua_public, &as_public);
        assert_eq!(cek, b("oIhVW04MRdy2XN9CiKLxTg"));
        assert_eq!(nonce, b("4h_95klXJ5E_qnoN"));

        // The whole message, header and record, byte for byte.
        let body = encrypt_with(
            &ecdh_secret,
            &auth,
            &salt,
            &ua_public,
            &as_public,
            &plaintext,
        )
        .unwrap();
        assert_eq!(
            body,
            b(
                "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml
               mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT
               pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
            )
        );
        // 86 header bytes, 41 of text, the delimiter and a 16-byte tag. (The example's
        // "Content-Length: 145" counts one too many.)
        assert_eq!(body.len(), 86 + 41 + 1 + 16);
    }

    /// Encrypts to a fresh key, as for a real phone, and decrypts as the browser would.
    #[test]
    fn a_message_decrypts_with_the_subscription_key() {
        let rng = SystemRandom::new();
        let ua = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let ua_public = ua.compute_public_key().unwrap().as_ref().to_vec();
        let auth = [7u8; 16];
        let body = encrypt(&ua_public, &auth, b"hello phone").unwrap();
        let salt = &body[..16];
        assert_eq!(&body[16..20], &4096u32.to_be_bytes());
        let idlen = body[20] as usize;
        let as_public = &body[21..21 + idlen];
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
        let secret = agreement::agree_ephemeral(ua, &peer, |s| s.to_vec()).unwrap();
        let (cek, nonce) = derive(&secret, &auth, salt, &ua_public, as_public);
        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
        let mut rec = body[21 + idlen..].to_vec();
        let plain = key
            .open_in_place(
                aead::Nonce::try_assume_unique_for_key(&nonce).unwrap(),
                aead::Aad::empty(),
                &mut rec,
            )
            .unwrap();
        assert_eq!(plain, b"hello phone\x02");
    }

    #[test]
    fn signs_a_vapid_token_for_the_push_service() {
        let dir = TempDir::new();
        let file = dir.path().join("webpush-key.json");
        let key = key_at(&file).unwrap();
        // Made once, kept private, and the same key next time.
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&file).unwrap().permissions().mode()
        };
        assert_eq!(mode & 0o077, 0, "the key is private");
        assert_eq!(
            key_at(&file).unwrap().public_key().as_ref(),
            key.public_key().as_ref()
        );
        assert_eq!(key.public_key().as_ref().len(), 65);

        let h = vapid_header(&key, "https://push.example.net:8443/push/abc?x=1").unwrap();
        let (t, k) = h
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        assert_eq!(unb64(k).unwrap(), key.public_key().as_ref());
        let parts: Vec<&str> = t.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value = serde_json::from_slice(&unb64(parts[0]).unwrap()).unwrap();
        assert_eq!(header, serde_json::json!({"typ": "JWT", "alg": "ES256"}));
        let claims: serde_json::Value = serde_json::from_slice(&unb64(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://push.example.net:8443");
        assert_eq!(claims["sub"], CONTACT);
        let now = time::now_ms() as u64 / 1000;
        let exp = claims["exp"].as_u64().unwrap();
        assert!(exp > now && exp <= now + 24 * 3600, "{exp} {now}");
        // ES256: a raw 64-byte r||s signature over header.claims, by the public key.
        let sig = unb64(parts[2]).unwrap();
        assert_eq!(sig.len(), 64);
        let signed = format!("{}.{}", parts[0], parts[1]);
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            key.public_key().as_ref(),
        )
        .verify(signed.as_bytes(), &sig)
        .unwrap();
        let mut bad = sig.clone();
        bad[5] ^= 1;
        assert!(ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            key.public_key().as_ref(),
        )
        .verify(signed.as_bytes(), &bad)
        .is_err());
    }

    #[test]
    fn keeps_several_devices_and_forgets_gone_ones() {
        let dir = TempDir::new();
        let file = dir.path().join("webpush-subs.json");
        let p256dh = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
        let auth = "BTBZMqHH6r4Tts7J_aSIgg";
        let a = validate(
            "https://fcm.googleapis.com/fcm/send/a",
            p256dh,
            auth,
            "Android",
        )
        .unwrap();
        // Padded, standard-alphabet base64 is accepted too, and stored as base64url.
        let padded = "BTBZMqHH6r4Tts7J/aSIgg==";
        let bb = validate(
            "https://web.push.apple.com/b",
            p256dh,
            padded,
            "iPhone<script>",
        )
        .unwrap();
        assert_eq!(bb.auth, auth);
        assert_eq!(bb.device, "iPhonescript");
        assert_eq!(add_at(&file, a.clone()).unwrap(), 1);
        assert_eq!(add_at(&file, bb.clone()).unwrap(), 2);
        // The same endpoint again replaces it.
        assert_eq!(add_at(&file, a.clone()).unwrap(), 2);
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&file).unwrap().permissions().mode()
        };
        assert_eq!(mode & 0o077, 0, "subscriptions are private");
        let subs = load_at(&file);
        assert_eq!(subs[0].endpoint, bb.endpoint);
        assert_eq!(subs[1].endpoint, a.endpoint);
        assert_eq!(remove_at(&file, &[a.endpoint.as_str()]).unwrap(), 1);
        assert_eq!(remove_at(&file, &["https://nope"]).unwrap(), 0);
        assert_eq!(load_at(&file), vec![bb]);
        for i in 0..25 {
            let s = validate(&format!("https://push.example/{i}"), p256dh, auth, "").unwrap();
            add_at(&file, s).unwrap();
        }
        let subs = load_at(&file);
        assert_eq!(subs.len(), MAX_DEVICES);
        assert_eq!(subs.last().unwrap().endpoint, "https://push.example/24");

        // What a subscription has to look like.
        assert!(validate("http://push.example/x", p256dh, auth, "").is_err());
        assert!(validate("http://127.0.0.1:9/x", p256dh, auth, "").is_ok());
        assert!(validate("file:///etc/passwd", p256dh, auth, "").is_err());
        assert!(validate("https://p/x", "BCVx", auth, "").is_err());
        assert!(validate("https://p/x", p256dh, "AAAA", "").is_err());
    }

    #[test]
    fn the_payload_has_the_title_text_and_link() {
        let p: serde_json::Value = serde_json::from_str(&payload(
            "box:w",
            "amux: w",
            &"x".repeat(5000),
            Some("https://h/#run=box%3Aw"),
        ))
        .unwrap();
        assert_eq!(p["title"], "amux: w");
        assert_eq!(p["url"], "https://h/#run=box%3Aw");
        assert_eq!(p["tag"], "box:w");
        assert_eq!(p["body"].as_str().unwrap().chars().count(), MAX_BODY_CHARS);
    }
}
