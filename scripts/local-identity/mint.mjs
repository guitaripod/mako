const encoder = new TextEncoder();
const KID = "local-test-key";
const SLUG = process.argv[2] ?? "dream-eater";
const MIDGAR_ID = "0194F3A2-7B1C-4D2E-9A60-3C5E8F1B2D47";
const SECOND_ID = "0194F3A2-7B1C-4D2E-9A60-3C5E8F1B2D48";
const NATIVE_ID = "0194F3A2-7B1C-4D2E-9A60-3C5E8F1B2D49";
const DEVELOPMENT_ID = "0194F3A2-7B1C-4D2E-9A60-3C5E8F1B2D4A";
const UNKNOWN_ID = "0194F3A2-7B1C-4D2E-9A60-3C5E8F1B2D4B";

function base64Url(bytes) {
  return Buffer.from(bytes).toString("base64url");
}

async function throwawayPair() {
  return crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
}

async function publicJwk(pair, kid) {
  const { kty, crv, x, y } = await crypto.subtle.exportKey("jwk", pair.publicKey);
  return { kty, crv, x, y, kid, alg: "ES256", use: "sig" };
}

/**
 * Signs claims exactly as services/identity does: an ES256 JWS with `typ` JWT and the key's `kid`, signature in raw r||s.
 */
async function sign(pair, kid, claims) {
  const header = { alg: "ES256", typ: "JWT", kid };
  const input = `${base64Url(encoder.encode(JSON.stringify(header)))}.${base64Url(encoder.encode(JSON.stringify(claims)))}`;
  const signature = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, pair.privateKey, encoder.encode(input));
  return `${input}.${base64Url(new Uint8Array(signature))}`;
}

function claims(issuedAt, overrides = {}) {
  return {
    iss: "midgar-identity",
    aud: "midgar",
    sub: MIDGAR_ID,
    app: SLUG,
    att: "app_attest",
    iat: issuedAt,
    exp: issuedAt + 3600,
    ...overrides,
  };
}

const now = Math.floor(Date.now() / 1000);
const pair = await throwawayPair();
const stranger = await throwawayPair();
const valid = await sign(pair, KID, claims(now));
const [header, payload, signature] = valid.split(".");
const forgedPayload = base64Url(encoder.encode(JSON.stringify(claims(now, { sub: MIDGAR_ID.replace("0194", "0195") }))));

process.stdout.write(
  JSON.stringify({
    jwks: { keys: [await publicJwk(pair, KID)] },
    tokens: {
      valid,
      development: await sign(pair, KID, claims(now, { att: "development" })),
      expired: await sign(pair, KID, claims(now - 7200)),
      wrong_app: await sign(pair, KID, claims(now, { app: "pay-day" })),
      wrong_issuer: await sign(pair, KID, claims(now, { iss: "someone-else" })),
      tampered: `${header}.${forgedPayload}.${signature}`,
      other_key: await sign(stranger, KID, claims(now)),
      unknown_kid: await sign(stranger, "not-published", claims(now)),
      second: await sign(pair, KID, claims(now, { sub: SECOND_ID })),
      native: await sign(pair, KID, claims(now, { sub: NATIVE_ID })),
      native_development: await sign(pair, KID, claims(now, { sub: DEVELOPMENT_ID, att: "development" })),
      native_wrong_app: await sign(pair, KID, claims(now, { sub: NATIVE_ID, app: "pay-day" })),
      native_expired: await sign(pair, KID, claims(now - 7200, { sub: NATIVE_ID })),
    },
    ids: { first: MIDGAR_ID, second: SECOND_ID, native: NATIVE_ID, development: DEVELOPMENT_ID, unknown: UNKNOWN_ID },
  }),
);
