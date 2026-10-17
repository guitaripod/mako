/**
 * Stands in for the `midgar-identity` worker in the local end-to-end run: serves the public key set the test
 * script generated, which arrives in the `JWKS` variable. Nothing else of identity exists here.
 */
export default {
  fetch(request, env) {
    const { pathname } = new URL(request.url);
    if (pathname !== "/.well-known/jwks.json") {
      return new Response("not found", { status: 404 });
    }
    return new Response(env.JWKS, { headers: { "content-type": "application/json" } });
  },
};
