// Local development only; see wrangler.toml.
export default {
  async fetch(request, env) {
    const path = new URL(request.url).pathname;
    // Cron triggers do not fire locally; this runs the pipeline's sweep.
    if (path === "/__sweep") {
      await env.PIPELINE.scheduled({ cron: "* * * * *" });
      return new Response("swept\n");
    }
    // With STOCK_SHORT_NAME set (dev/local.sh --stock-short-name), the routes a
    // deployment adds for the stock layout; see docs/deploy.md.
    const short = env.STOCK_SHORT_NAME;
    if (short) {
      if (path.startsWith(`/${short}_cgi/cgi`)) return env.SCHEDULER.fetch(request);
      if (path.startsWith(`/${short}_cgi/file_upload_handler`) || path.startsWith(`/${short}/download/`)) {
        return env.FILES.fetch(request);
      }
    }
    if (path.startsWith("/sched")) return env.SCHEDULER.fetch(request);
    if (path.startsWith("/download/") || path.startsWith("/upload")) return env.FILES.fetch(request);
    if (path.startsWith("/admin/")) return env.ADMIN.fetch(request);
    return env.WEB.fetch(request);
  },
};
