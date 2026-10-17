use js_sys::{Array, Object, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Crypto, CryptoKey, SubtleCrypto};

use super::keys::Jwk;
use super::SignatureCheck;

/// Checks signatures with the runtime's `SubtleCrypto`, which runs native code and so spends no meaningful
/// CPU of the 10 ms a Workers Free request gets.
pub struct SubtleCheck;

impl SignatureCheck for SubtleCheck {
    async fn valid(&self, jwk: &Jwk, signature: &[u8], input: &[u8]) -> bool {
        verify_es256(jwk, signature, input).await.unwrap_or(false)
    }
}

/// The global `crypto.subtle`.
fn subtle() -> Result<SubtleCrypto, JsValue> {
    let crypto: Crypto = Reflect::get(&js_sys::global(), &JsValue::from_str("crypto"))?.unchecked_into();
    Ok(crypto.subtle())
}

/// A plain JS object with the given string members.
fn object(members: &[(&str, &str)]) -> Result<Object, JsValue> {
    let object = Object::new();
    for (name, value) in members {
        Reflect::set(&object, &JsValue::from_str(name), &JsValue::from_str(value))?;
    }
    Ok(object)
}

/// Imports the public key and verifies an ES256 signature in the raw `r || s` form a JWS carries, which is also
/// what WebCrypto expects, so no DER conversion is involved.
async fn verify_es256(jwk: &Jwk, signature: &[u8], input: &[u8]) -> Result<bool, JsValue> {
    let subtle = subtle()?;
    let key_data = object(&[("kty", &jwk.kty), ("crv", &jwk.crv), ("x", &jwk.x), ("y", &jwk.y)])?;
    let curve = object(&[("name", "ECDSA"), ("namedCurve", "P-256")])?;
    let usages = Array::of1(&JsValue::from_str("verify"));
    let key: CryptoKey = JsFuture::from(subtle.import_key_with_object("jwk", &key_data, &curve, false, &usages)?)
        .await?
        .unchecked_into();
    let algorithm = object(&[("name", "ECDSA"), ("hash", "SHA-256")])?;
    let verdict = JsFuture::from(subtle.verify_with_object_and_u8_array_and_u8_array(&algorithm, &key, signature, input)?).await?;
    Ok(verdict.as_bool().unwrap_or(false))
}
