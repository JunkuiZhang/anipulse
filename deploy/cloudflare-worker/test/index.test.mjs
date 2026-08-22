import assert from "node:assert/strict";
import test from "node:test";

import worker, { allowedClientIps, resolveRoute } from "../src/index.js";

const CLIENT_IP = "203.0.113.10";

test("parses a comma-separated IP allowlist", () => {
  assert.deepEqual(allowedClientIps(" 203.0.113.10,2001:db8::1,203.0.113.10 "), [
    "203.0.113.10",
    "2001:db8::1",
  ]);
});

test("only exposes the exact AniPulse upstream routes", () => {
  const episodes = resolveRoute(new URL(
    "https://proxy.example.com/bangumi/v0/episodes?subject_id=622206&type=0&limit=1&offset=8",
  ));
  assert.equal(
    episodes.upstreamUrl,
    "https://api.bgm.tv/v0/episodes?subject_id=622206&type=0&limit=1&offset=8",
  );
  assert.equal(resolveRoute(new URL("https://proxy.example.com/data.json?url=evil")), null);
  assert.equal(resolveRoute(new URL("https://proxy.example.com/bangumi/v0/users/me")), null);
  assert.equal(resolveRoute(new URL(
    "https://proxy.example.com/bangumi/v0/episodes?subject_id=622206&type=0&limit=100&offset=0",
  )), null);
});

test("fails closed when the allowlist is missing and rejects other clients", async () => {
  const request = authorizedRequest("https://proxy.example.com/healthz");
  assert.equal((await worker.fetch(request, {}, {})).status, 503);
  assert.equal((await worker.fetch(request, { ALLOWED_CLIENT_IPS: "198.51.100.2" }, {})).status, 403);
});

test("proxies and caches bangumi-data", async () => {
  const cache = memoryCache();
  const calls = [];
  const runtimeFetch = async (url, init) => {
    calls.push({ url, init });
    return new Response('{"items":[]}', {
      headers: { "Content-Type": "application/json" },
    });
  };
  const context = immediateContext();
  const env = { ALLOWED_CLIENT_IPS: CLIENT_IP };
  const request = authorizedRequest("https://proxy.example.com/data.json");

  const first = await worker.fetch(request, env, context, { fetch: runtimeFetch, cache });
  assert.equal(first.status, 200);
  assert.equal(first.headers.get("X-AniPulse-Proxy-Cache"), "MISS");
  assert.equal(calls[0].url, "https://unpkg.com/bangumi-data@0.3/dist/data.json");
  await context.flush();

  const second = await worker.fetch(request, env, immediateContext(), { fetch: runtimeFetch, cache });
  assert.equal(second.headers.get("X-AniPulse-Proxy-Cache"), "HIT");
  assert.equal(calls.length, 1);
});

test("follows cover redirects inside the Worker and rejects oversized images", async () => {
  let fetchInit;
  const env = { ALLOWED_CLIENT_IPS: CLIENT_IP };
  const request = authorizedRequest(
    "https://proxy.example.com/bangumi/v0/subjects/622206/image?type=medium",
  );
  const response = await worker.fetch(request, env, immediateContext(), {
    cache: null,
    fetch: async (_url, init) => {
      fetchInit = init;
      return new Response(new Uint8Array([0xff, 0xd8, 0xff]), {
        headers: { "Content-Type": "image/jpeg" },
      });
    },
  });
  assert.equal(response.status, 200);
  assert.equal(fetchInit.redirect, "follow");
  assert.match(response.headers.get("Cache-Control"), /s-maxage=2592000/);

  const oversized = await worker.fetch(request, env, immediateContext(), {
    cache: null,
    fetch: async () => new Response("not downloaded", {
      headers: {
        "Content-Type": "image/jpeg",
        "Content-Length": String(5 * 1024 * 1024 + 1),
      },
    }),
  });
  assert.equal(oversized.status, 502);
});

function authorizedRequest(url, method = "GET") {
  return new Request(url, {
    method,
    headers: { "CF-Connecting-IP": CLIENT_IP },
  });
}

function memoryCache() {
  const values = new Map();
  return {
    async match(request) {
      return values.get(request.url)?.clone();
    },
    async put(request, response) {
      values.set(request.url, response.clone());
    },
  };
}

function immediateContext() {
  const pending = [];
  return {
    waitUntil(promise) {
      pending.push(promise);
    },
    async flush() {
      await Promise.all(pending);
    },
  };
}
