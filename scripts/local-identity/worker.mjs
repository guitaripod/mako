/**
 * Stands in for the `midgar-identity` worker in the local end-to-end run: serves the public key set the test
 * script generated (the `JWKS` variable) and answers `GET /internal/exists/<id>` for the ids in `KNOWN_IDS`
 * (comma separated, lowercase) when `X-Midgar-Internal` carries `LOOKUP_SECRET`. Nothing else of identity exists here.
 */
export default {
  fetch(request, env) {
    const { pathname } = new URL(request.url);
    if (pathname === "/.well-known/jwks.json") {
      return new Response(env.JWKS, { headers: { "content-type": "application/json" } });
    }
    const prefix = "/internal/exists/";
    if (pathname.startsWith(prefix) && request.method === "GET" && env.LOOKUP_SECRET) {
      if (request.headers.get("X-Midgar-Internal") !== env.LOOKUP_SECRET) {
        return new Response("unauthorized", { status: 401 });
      }
      const known = (env.KNOWN_IDS ?? "").split(",").filter(Boolean);
      const exists = known.includes(pathname.slice(prefix.length).toLowerCase());
      return new Response(JSON.stringify({ exists }), { headers: { "content-type": "application/json" } });
    }
    return new Response("not found", { status: 404 });
  },
};
