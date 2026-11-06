/**
 * Stands in for the `midgar-ledger` worker in the local end-to-end run. Both routes need `X-Midgar-Internal` to carry
 * `VERIFY_SECRET`, and neither uses any Apple certificate chain.
 *
 * `POST /internal/verify-transaction`: a `signedTransaction` that is `stub.<base64url of a JSON object>` verifies and
 * answers that object's fields (with the defaults a real transaction has), `stub-down` answers 503 as an unavailable
 * ledger does, and anything else is a payload that does not verify (400).
 *
 * `POST /internal/entitlements`: a `midgarId` listed in `PREMIUM_IDS` holds Dream Eater's `premium`, one listed in
 * `DOWN_IDS` gets the 503 of a ledger that cannot answer, and any other holds nothing.
 */
const DEFAULTS = { ok: true, quantity: 1, revocationDate: null, inAppOwnershipType: "PURCHASED", registrySlug: null };

function decode(signed) {
  if (typeof signed !== "string" || !signed.startsWith("stub.")) return null;
  try {
    const base64 = signed.slice("stub.".length).replaceAll("-", "+").replaceAll("_", "/");
    const text = new TextDecoder().decode(Uint8Array.from(atob(base64), (c) => c.charCodeAt(0)));
    const fields = JSON.parse(text);
    return fields !== null && typeof fields === "object" && !Array.isArray(fields) ? { ...DEFAULTS, ...fields } : null;
  } catch {
    return null;
  }
}

function listed(ids, midgarId) {
  return (ids ?? "").split(",").map((id) => id.trim().toLowerCase()).filter(Boolean).includes(String(midgarId ?? "").toLowerCase());
}

async function verifyTransaction(request) {
  const { signedTransaction } = await request.json();
  if (signedTransaction === "stub-down") return new Response("unavailable", { status: 503 });
  const fields = decode(signedTransaction);
  if (fields === null) {
    return Response.json({ error: { code: "unverifiable_payload", message: "The signed payload did not verify." } }, { status: 400 });
  }
  return Response.json(fields);
}

async function entitlements(request, env) {
  const { midgarId } = await request.json();
  if (listed(env.DOWN_IDS, midgarId)) return new Response("unavailable", { status: 503 });
  if (listed(env.PREMIUM_IDS, midgarId)) return Response.json({ entitlements: [{ app: "dream-eater", entitlement: "premium" }] });
  return Response.json({ entitlements: [] });
}

const ROUTES = { "/internal/verify-transaction": verifyTransaction, "/internal/entitlements": entitlements };

export default {
  async fetch(request, env) {
    const route = ROUTES[new URL(request.url).pathname];
    if (route === undefined) return new Response("not found", { status: 404 });
    if (request.method !== "POST") return new Response("method not allowed", { status: 405 });
    if (!env.VERIFY_SECRET) return new Response("not found", { status: 404 });
    if (request.headers.get("X-Midgar-Internal") !== env.VERIFY_SECRET) return new Response("unauthorized", { status: 401 });
    return route(request, env);
  },
};
