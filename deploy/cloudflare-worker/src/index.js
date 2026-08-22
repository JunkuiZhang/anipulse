const BANGUMI_DATA_URL = "https://unpkg.com/bangumi-data@0.3/dist/data.json";
const BANGUMI_API_ORIGIN = "https://api.bgm.tv";

const ROUTES = {
  data: {
    edgeTtl: 6 * 60 * 60,
    clientTtl: 5 * 60,
    maximumBytes: 16 * 1024 * 1024,
    kind: "json",
  },
  episodes: {
    edgeTtl: 15 * 60,
    clientTtl: 60,
    maximumBytes: 2 * 1024 * 1024,
    kind: "json",
  },
  cover: {
    edgeTtl: 30 * 24 * 60 * 60,
    clientTtl: 24 * 60 * 60,
    maximumBytes: 5 * 1024 * 1024,
    kind: "image",
  },
};

export default {
  async fetch(request, env, context, dependencies) {
    return handleRequest(request, env, context, dependencies);
  },
};

export async function handleRequest(request, env = {}, context = {}, dependencies = {}) {
  const runtimeFetch = dependencies.fetch ?? globalThis.fetch;
  const cache = dependencies.cache ?? globalThis.caches?.default;
  const clientIp = request.headers.get("CF-Connecting-IP")?.trim();
  const allowlist = allowedClientIps(env.ALLOWED_CLIENT_IPS);

  if (!allowlist.length) {
    return errorResponse(503, "client IP allowlist is not configured");
  }
  if (!clientIp || !allowlist.includes(clientIp)) {
    return errorResponse(403, "forbidden");
  }
  if (!['GET', 'HEAD'].includes(request.method)) {
    return errorResponse(405, "method not allowed", { Allow: "GET, HEAD" });
  }

  const incomingUrl = new URL(request.url);
  if (incomingUrl.pathname === "/healthz") {
    return jsonResponse(200, {
      ok: true,
      service: "anipulse-bangumi-proxy",
    });
  }

  const route = resolveRoute(incomingUrl);
  if (!route) {
    return errorResponse(404, "not found");
  }

  const cacheKeyUrl = new URL(request.url);
  cacheKeyUrl.searchParams.set("__anipulse_cache", env.CACHE_VERSION?.trim() || "v1");
  const cacheKey = new Request(cacheKeyUrl, { method: "GET" });
  if (cache) {
    const cached = await cache.match(cacheKey);
    if (cached) {
      return responseForClient(cached, request.method === "HEAD", "HIT");
    }
  }

  let upstreamResponse;
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 15_000);
  try {
    const headers = new Headers({ Accept: route.accept });
    headers.set(
      "User-Agent",
      env.UPSTREAM_USER_AGENT?.trim()
        || request.headers.get("User-Agent")
        || "AniPulse/0.1 (personal self-hosted)",
    );
    upstreamResponse = await runtimeFetch(route.upstreamUrl, {
      method: "GET",
      headers,
      redirect: "follow",
      signal: controller.signal,
    });
  } catch (error) {
    const detail = error?.name === "AbortError" ? "upstream timeout" : "upstream unavailable";
    return errorResponse(504, detail);
  } finally {
    clearTimeout(timeout);
  }

  if (!upstreamResponse.ok) {
    return errorResponse(502, `upstream returned HTTP ${upstreamResponse.status}`);
  }

  const contentLength = Number(upstreamResponse.headers.get("content-length"));
  if (Number.isFinite(contentLength) && contentLength > route.maximumBytes) {
    return errorResponse(502, "upstream response is too large");
  }

  const contentType = upstreamResponse.headers.get("content-type")?.toLowerCase() || "";
  if (!validContentType(route.kind, contentType)) {
    return errorResponse(502, "upstream returned an unexpected content type");
  }

  let body;
  try {
    body = await upstreamResponse.arrayBuffer();
  } catch (_error) {
    return errorResponse(502, "upstream response could not be read");
  }
  if (body.byteLength > route.maximumBytes) {
    return errorResponse(502, "upstream response is too large");
  }

  const headers = new Headers({
    "Cache-Control": `public, max-age=${route.clientTtl}, s-maxage=${route.edgeTtl}`,
    "Content-Type": contentType,
    "X-Content-Type-Options": "nosniff",
  });
  for (const name of ["etag", "last-modified"]) {
    const value = upstreamResponse.headers.get(name);
    if (value) headers.set(name, value);
  }
  const cacheable = new Response(body, { status: 200, headers });
  if (cache) {
    const write = cache.put(cacheKey, cacheable.clone()).catch((error) => {
      console.warn("cache put failed", error);
    });
    if (typeof context.waitUntil === "function") context.waitUntil(write);
    else await write;
  }
  return responseForClient(cacheable, request.method === "HEAD", "MISS");
}

export function resolveRoute(url) {
  if (url.pathname === "/data.json" && [...url.searchParams].length === 0) {
    return {
      ...ROUTES.data,
      accept: "application/json",
      upstreamUrl: BANGUMI_DATA_URL,
    };
  }

  if (url.pathname === "/bangumi/v0/episodes" && validEpisodeQuery(url.searchParams)) {
    const upstream = new URL("/v0/episodes", BANGUMI_API_ORIGIN);
    upstream.search = url.search;
    return {
      ...ROUTES.episodes,
      accept: "application/json",
      upstreamUrl: upstream.toString(),
    };
  }

  const cover = url.pathname.match(/^\/bangumi\/v0\/subjects\/([1-9]\d*)\/image$/);
  if (cover && onlyMediumCoverQuery(url.searchParams)) {
    const upstream = new URL(`/v0/subjects/${cover[1]}/image`, BANGUMI_API_ORIGIN);
    upstream.searchParams.set("type", "medium");
    return {
      ...ROUTES.cover,
      accept: "image/avif,image/webp,image/*,*/*;q=0.8",
      upstreamUrl: upstream.toString(),
    };
  }

  return null;
}

export function allowedClientIps(value) {
  return [...new Set((value || "").split(",").map((item) => item.trim()).filter(Boolean))];
}

function validEpisodeQuery(parameters) {
  const allowed = new Set(["subject_id", "type", "limit", "offset"]);
  if ([...parameters.keys()].some((key) => !allowed.has(key))) return false;
  if ([...parameters.keys()].length !== 4) return false;
  return /^[1-9]\d*$/.test(parameters.get("subject_id") || "")
    && parameters.get("type") === "0"
    && parameters.get("limit") === "1"
    && /^(0|[1-9]\d*)$/.test(parameters.get("offset") || "");
}

function onlyMediumCoverQuery(parameters) {
  return [...parameters.keys()].length === 1 && parameters.get("type") === "medium";
}

function validContentType(kind, contentType) {
  if (kind === "image") return contentType.startsWith("image/");
  return contentType.includes("application/json") || contentType.includes("text/json");
}

function responseForClient(response, headOnly, cacheStatus) {
  const headers = new Headers(response.headers);
  headers.set("X-AniPulse-Proxy-Cache", cacheStatus);
  return new Response(headOnly ? null : response.body, {
    status: response.status,
    headers,
  });
}

function jsonResponse(status, body, extraHeaders = {}) {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      "Cache-Control": "no-store",
      "Content-Type": "application/json; charset=utf-8",
      "X-Content-Type-Options": "nosniff",
      ...extraHeaders,
    },
  });
}

function errorResponse(status, error, extraHeaders = {}) {
  return jsonResponse(status, { error }, extraHeaders);
}
