// node --test dev/router
import test from "node:test";
import assert from "node:assert/strict";
import { NATIVE_ROUTES, stockRoutes, pick, route } from "./routes.js";

const SHORTS = ["example", "scheduler", "sched_x", "uploadtest", "upload2", "downloads", "admin2", "a"];

test("without a stock layout the native routes decide", () => {
  const want = {
    "/sched": "SCHEDULER",
    "/upload/file_upload_handler": "FILES",
    "/download/wu_1_0": "FILES",
    "/admin/v1/status": "ADMIN",
    "/": "WEB",
    "/home.php": "WEB",
    "/scheduler/home.php": "SCHEDULER",
    "/uploadtest/home.php": "FILES",
  };
  for (const [path, target] of Object.entries(want)) assert.equal(route(path, ""), target, path);
});

for (const short of SHORTS) {
  test(`stock routes of ${short} send each URL to its Worker`, () => {
    const want = {
      [`/${short}_cgi/cgi`]: "SCHEDULER",
      [`/${short}_cgi/file_upload_handler`]: "FILES",
      [`/${short}/download/319/wu_1_0`]: "FILES",
      [`/${short}`]: "WEB",
      [`/${short}/`]: "WEB",
      [`/${short}/home.php`]: "WEB",
      [`/${short}/get_project_config.php`]: "WEB",
      // The native paths keep their Workers.
      "/sched": "SCHEDULER",
      "/upload/file_upload_handler": "FILES",
      "/download/wu_1_0": "FILES",
      "/admin/v1/status": "ADMIN",
      "/": "WEB",
      "/home.php": "WEB",
      "/get_project_config.php": "WEB",
    };
    for (const [path, target] of Object.entries(want)) assert.equal(route(path, short), target, `${short}: ${path}`);
  });
}

for (const short of SHORTS) {
  test(`query strings reach the same Workers for ${short}`, () => {
    const want = {
      [`/${short}_cgi/cgi?x=1`]: "SCHEDULER",
      [`/${short}_cgi/file_upload_handler?x=1`]: "FILES",
      [`/${short}/download/319/wu_1_0?x=1`]: "FILES",
      [`/${short}?x=1`]: "WEB",
      [`/${short}/?x=1`]: "WEB",
      [`/${short}/home.php?x=1`]: "WEB",
      "/sched?x=1": "SCHEDULER",
      "/upload/file_upload_handler?x=1": "FILES",
      "/download/wu_1_0?x=1": "FILES",
      "/admin/v1/status?x=1": "ADMIN",
      "/home.php?x=1": "WEB",
    };
    for (const [path, target] of Object.entries(want)) assert.equal(route(path, short), target, `${short}: ${path}`);
  });
}

test("the slashless page route has a wildcard, since the query string is part of the URL", () => {
  for (const short of SHORTS) {
    const slashless = stockRoutes(short).filter(([p]) => p === `/${short}*`);
    assert.equal(slashless.length, 1, short);
    // With an exact pattern the query string would miss it.
    const exact = [...NATIVE_ROUTES, [`/${short}`, "WEB"], [`/${short}/*`, "WEB"]];
    const captured = short.startsWith("sched") || short.startsWith("upload");
    assert.equal(pick(exact, `/${short}?x=1`) !== "WEB", captured, short);
  }
});

// The wildcard that also matches `<prefix>/...` and `<prefix>anything` never beats
// a route it should lose to, because those have a longer literal part.
test("the page route does not capture the endpoints of its neighbours", () => {
  const routes = [
    ...NATIVE_ROUTES,
    ["/x*", "WEB"], ["/x/*", "WEB"],
    ["/x_cgi/cgi*", "SCHEDULER"], ["/x_cgi/file_upload_handler*", "FILES"],
    ["/a*", "WEB"], ["/a/*", "WEB"], ["/a/download/*", "FILES"],
  ];
  const want = {
    "/x_cgi/cgi": "SCHEDULER", "/x_cgi/cgi?x=1": "SCHEDULER",
    "/x_cgi/file_upload_handler": "FILES",
    "/x": "WEB", "/x?x=1": "WEB", "/x/home.php": "WEB", "/xfoo": "WEB",
    "/a/download/1/wu": "FILES", "/a/download/1/wu?x=1": "FILES",
    "/a": "WEB", "/a/home.php?x=1": "WEB",
    "/admin/v1/status": "ADMIN", "/sched": "SCHEDULER",
  };
  for (const [path, target] of Object.entries(want)) assert.equal(pick(routes, path), target, path);
});

test("a short name that begins with a native prefix needs the page routes", () => {
  for (const short of SHORTS) {
    const withoutPages = [...NATIVE_ROUTES, ...stockRoutes(short).filter(([p]) => !p.startsWith(`/${short}/`) && p !== `/${short}*`)];
    const captured = short.startsWith("sched") || short.startsWith("upload");
    assert.equal(pick(withoutPages, `/${short}/home.php`) !== "WEB", captured, short);
  }
});

test("equal patterns for different Workers are a tie", () => {
  assert.equal(pick([["/x/*", "WEB"], ["/x/*", "FILES"]], "/x/a"), null);
  assert.equal(pick([["/x/*", "WEB"], ["/x/*", "WEB"]], "/x/a"), "WEB");
});
