---
title: Trailing-Slash Tolerance
description: /things and /things/ are treated as the same endpoint when no exact mock distinguishes them.
---

`/things` and `/things/` are the same endpoint as far as most APIs (and most clients) are concerned, but a mock registered for one used to 404 the other. Mimic falls back to the other form automatically, in both directions, whenever the literal request path matches nothing at all:

```bash
# Only /things is registered...
curl http://localhost:8080/things    # 200 — the exact match
curl http://localhost:8080/things/   # 200 too — falls back, no redirect
```

## Rules

- **Exact matches always win.** The fallback only ever runs once the literal path — as an exact key *and* against every [pattern route](/matching/path-parameters/) — matched nothing. Register `/things` **and** `/things/` as separate mocks and each serves only its own exact path; neither one falls back to the other.
- **Works both directions**, and for pattern routes too: a mock at `/users/:id` matches a request to `/users/42/` just as it matches `/users/42`.
- **The root path (`/`) is never touched.** There's no other form to fall back to.
- **Served directly, with no redirect** — a 30x round trip before the real response is exactly what this avoids.
- **The request log stays transparent about it.** `GET /admin/requests` (and the dashboard's match explanation) always show the path actually requested, and note when the winning mock was only reached by trying its trailing-slash variant:

  ```
  matched mocks/things.json (score 1000: method+path 1000, trailing slash normalized)
  ```

- **Opt out with `MIMIC_STRICT_TRAILING_SLASH=true`** to restore exact-path behavior — `/things` and `/things/` are entirely distinct paths again, no fallback tried either way.
- Applies everywhere path matching happens: body-matcher evaluation, the "does this request need its body read" check, and [CORS preflight](/guides/cors/) detection all use the same fallback, so a preflight for `/things/` is answered exactly when a real `GET /things/` would be.

## Interaction with match priority

The fallback only kicks in after the literal path has already been checked, so it never changes which mock wins when both the requested path and a candidate mock's path already agree — see [Match Priority](/matching/priority/) for the full scoring rules.
