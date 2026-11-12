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

test("a short name that begins with a native prefix needs the page routes", () => {
  for (const short of SHORTS) {
    const withoutPages = [...NATIVE_ROUTES, ...stockRoutes(short).filter(([p]) => !p.startsWith(`/${short}/`) && p !== `/${short}`)];
    const captured = short.startsWith("sched") || short.startsWith("upload");
    assert.equal(pick(withoutPages, `/${short}/home.php`) !== "WEB", captured, short);
  }
});

test("equal patterns for different Workers are a tie", () => {
  assert.equal(pick([["/x/*", "WEB"], ["/x/*", "FILES"]], "/x/a"), null);
  assert.equal(pick([["/x/*", "WEB"], ["/x/*", "WEB"]], "/x/a"), "WEB");
});
