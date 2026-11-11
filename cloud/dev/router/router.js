// Local development only; see wrangler.toml.
export default {
  async fetch(request, env) {
    const path = new URL(request.url).pathname;
    // Cron triggers do not fire locally; this runs the pipeline's sweep.
    if (path === "/__sweep") {
      await env.PIPELINE.scheduled({ cron: "* * * * *" });
      return new Response("swept\n");
    }
    if (path.startsWith("/sched")) return env.SCHEDULER.fetch(request);
    if (path.startsWith("/download/") || path.startsWith("/upload")) return env.FILES.fetch(request);
    if (path.startsWith("/admin/")) return env.ADMIN.fetch(request);
    return env.WEB.fetch(request);
  },
};
