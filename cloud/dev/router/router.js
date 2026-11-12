// Local development only; see wrangler.toml.
import { route } from "./routes.js";

export default {
  async fetch(request, env) {
    const path = new URL(request.url).pathname;
    // Cron triggers do not fire locally; this runs the pipeline's sweep.
    if (path === "/__sweep") {
      await env.PIPELINE.scheduled({ cron: "* * * * *" });
      return new Response("swept\n");
    }
    // With STOCK_SHORT_NAME set (dev/local.sh --stock-short-name), the routes a
    // deployment adds for the stock layout; see docs/deploy.md and routes.js.
    const target = route(path, env.STOCK_SHORT_NAME);
    if (target === null) return new Response("ambiguous route\n", { status: 500 });
    return env[target].fetch(request);
  },
};
