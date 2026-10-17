/**
 * Stands in for the `midgar-ledger` worker in the local end-to-end run. It answers `POST /internal/verify-transaction`
 * when `X-Midgar-Internal` carries `VERIFY_SECRET`, without any Apple certificate chain: a `signedTransaction` that is
 * `stub.<base64url of a JSON object>` verifies and answers that object's fields (with the defaults a real transaction
 * has), `stub-down` answers 503 as an unavailable ledger does, and anything else is a payload that does not verify (400).
 * Nothing else of the ledger exists here.
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

export default {
  async fetch(request, env) {
    const { pathname } = new URL(request.url);
    if (pathname !== "/internal/verify-transaction") return new Response("not found", { status: 404 });
    if (request.method !== "POST") return new Response("method not allowed", { status: 405 });
    if (!env.VERIFY_SECRET) return new Response("not found", { status: 404 });
    if (request.headers.get("X-Midgar-Internal") !== env.VERIFY_SECRET) return new Response("unauthorized", { status: 401 });
    const { signedTransaction } = await request.json();
    if (signedTransaction === "stub-down") return new Response("unavailable", { status: 503 });
    const fields = decode(signedTransaction);
    if (fields === null) {
      return Response.json({ error: { code: "unverifiable_payload", message: "The signed payload did not verify." } }, { status: 400 });
    }
    return Response.json(fields);
  },
};
