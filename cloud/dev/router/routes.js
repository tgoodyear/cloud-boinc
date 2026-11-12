// The routes of a deployment as path patterns, and how Cloudflare picks one:
// a pattern is matched against the URL after the host, query string included,
// so a wildcard has to cover it. Of the patterns that match, the one with the
// longest literal part wins, and an exact pattern beats a wildcard one of the
// same length. The
// native routes are the ones in the Workers' wrangler.toml files; the stock
// layout adds the ones in `StockLayout::routes` (cloud/crates/cb-core/src/stock.rs)
// for the make_project default prefixes of a short name. The Rust test
// `stock_routes_beat_the_native_ones_they_overlap` holds the same table.

export const NATIVE_ROUTES = [
  ["/sched*", "SCHEDULER"],
  ["/upload*", "FILES"],
  ["/download/*", "FILES"],
  ["/admin/*", "ADMIN"],
  ["/*", "WEB"],
];

export function stockRoutes(short) {
  if (!short) return [];
  return [
    [`/${short}_cgi/cgi*`, "SCHEDULER"],
    [`/${short}_cgi/file_upload_handler*`, "FILES"],
    [`/${short}/download/*`, "FILES"],
    [`/${short}*`, "WEB"],
    [`/${short}/*`, "WEB"],
  ];
}

// The binding a path (with its query string, if any) goes to, or null for a
// tie between Workers.
export function pick(routes, path) {
  let best = null;
  let tie = false;
  for (const [pattern, target] of routes) {
    const wildcard = pattern.endsWith("*");
    const literal = wildcard ? pattern.slice(0, -1) : pattern;
    if (wildcard ? !path.startsWith(literal) : path !== literal) continue;
    const key = [literal.length, wildcard ? 0 : 1];
    if (best && best.key[0] === key[0] && best.key[1] === key[1]) {
      tie = tie || best.target !== target;
    } else if (!best || key[0] > best.key[0] || (key[0] === best.key[0] && key[1] > best.key[1])) {
      best = { key, target };
      tie = false;
    }
  }
  return tie || !best ? null : best.target;
}

export function route(path, short) {
  return pick([...NATIVE_ROUTES, ...stockRoutes(short)], path);
}
