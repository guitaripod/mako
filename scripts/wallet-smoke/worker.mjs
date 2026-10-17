import mako, { Wallet } from "../../build/worker/shim.mjs";

export { Wallet };

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    const [, name, ...rest] = url.pathname.split("/");
    if (!name) {
      return new Response("usage: /<midgar id>/<wallet path>", { status: 400 });
    }
    const stub = env.WALLET.get(env.WALLET.idFromName(name));
    const inner = new URL(`/${rest.join("/")}${url.search}`, "https://wallet.internal");
    return stub.fetch(new Request(inner, request));
  },
};
