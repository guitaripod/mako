use js_sys::{Array, Object, Reflect, Uint8Array};
use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Crypto, CryptoKey};
use worker::{Fetch, Headers, Method, Request, RequestInit};

use super::{Answer, Call, Signer, Transport, Verb};

/// Sends calls to Google with the runtime's `fetch`.
pub struct FetchTransport;

impl Transport for FetchTransport {
    async fn send(&self, call: &Call) -> Result<Answer, String> {
        let failed = |e: worker::Error| e.to_string();
        let headers = Headers::new();
        if let Some(bearer) = &call.bearer {
            headers.set("Authorization", &format!("Bearer {bearer}")).map_err(failed)?;
        }
        if let Some(content_type) = call.content_type {
            headers.set("Content-Type", content_type).map_err(failed)?;
        }
        let mut init = RequestInit::new();
        init.with_method(match call.verb {
            Verb::Get => Method::Get,
            Verb::Post => Method::Post,
        })
        .with_headers(headers);
        if let Some(body) = &call.body {
            init.with_body(Some(JsValue::from_str(body)));
        }
        let request = Request::new_with_init(&call.url, &init).map_err(failed)?;
        let mut response = Fetch::Request(request).send().await.map_err(failed)?;
        let status = response.status_code();
        let body = response.json::<Value>().await.unwrap_or(Value::Null);
        Ok(Answer { status, body })
    }
}

/// Signs with the runtime's `SubtleCrypto`, which runs native code and so spends no meaningful CPU of the request's
/// budget on the RSA operation.
pub struct SubtleSigner;

impl Signer for SubtleSigner {
    async fn sign(&self, pkcs8: &[u8], input: &[u8]) -> Result<Vec<u8>, String> {
        sign_rs256(pkcs8, input).await.map_err(|e| js_message(&e))
    }
}

/// The message of a thrown JS value (a `DOMException` carries it in `message`), for the log.
fn js_message(error: &JsValue) -> String {
    error
        .as_string()
        .or_else(|| Reflect::get(error, &JsValue::from_str("message")).ok().and_then(|message| message.as_string()))
        .unwrap_or_else(|| "SubtleCrypto failed".to_string())
}

/// Imports the PKCS #8 key for RSASSA-PKCS1-v1_5 with SHA-256 and signs the input with it.
async fn sign_rs256(pkcs8: &[u8], input: &[u8]) -> Result<Vec<u8>, JsValue> {
    let crypto: Crypto = Reflect::get(&js_sys::global(), &JsValue::from_str("crypto"))?.unchecked_into();
    let subtle = crypto.subtle();
    let algorithm = Object::new();
    Reflect::set(&algorithm, &JsValue::from_str("name"), &JsValue::from_str("RSASSA-PKCS1-v1_5"))?;
    Reflect::set(&algorithm, &JsValue::from_str("hash"), &JsValue::from_str("SHA-256"))?;
    let usages = Array::of1(&JsValue::from_str("sign"));
    let key_data = Uint8Array::from(pkcs8);
    let key: CryptoKey = JsFuture::from(subtle.import_key_with_object("pkcs8", &key_data, &algorithm, false, &usages)?)
        .await?
        .unchecked_into();
    let signature = JsFuture::from(subtle.sign_with_object_and_u8_array(&algorithm, &key, input)?).await?;
    Ok(Uint8Array::new(&signature).to_vec())
}
