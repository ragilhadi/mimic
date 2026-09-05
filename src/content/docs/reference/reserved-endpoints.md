---
title: Reserved Endpoints
description: The routes Mimic answers itself, and how to free them for your own mocks.
---

Mimic answers a handful of routes itself, and they are matched **ahead of** the mock set. A mock declaring one of these loads normally, is listed by `GET /admin/mocks`, and then never serves a request:

| Reserved | Method(s) |
|---|---|
| `/health` | `GET` |
| `/admin/dashboard` | `GET` |
| `/admin/requests` | `GET`, `DELETE` |
| `/admin/mocks` | `GET` |
| `/admin/sequences` | `GET` |
| `/admin/sequences/reset` | `POST` |
| `/admin/scenario` | `GET`, `POST` |

Only these exact **method + path** pairs are reserved. `POST /health` and `GET /admin/users` reach the mock set normally and can be mocked.

## Collisions are reported, not silent

- The loader warns at startup, and again whenever [hot reload](/guides/hot-reload/) introduces one, naming the file — `mocks/health_down.json declares GET /health, which is reserved by Mimic's health check and will never be served`.
- `GET /admin/mocks` reports the mock with `"reachable": false` and an `unreachable_reason`, and the [dashboard](/guides/admin-dashboard/) shows it as an **unreachable** badge — so a permanent `hits: 0` is distinguishable from "nothing has called it yet".

## Freeing a reserved path

If your API genuinely owns one of these paths, take it back:

```bash
MIMIC_HEALTH_PATH= mimic                    # no health check; GET /health is yours
MIMIC_HEALTH_PATH=/_mimic/health mimic      # or move it out of the way
MIMIC_ADMIN_PREFIX=/_mimic mimic            # admin API at /_mimic/mocks, etc.
MIMIC_DISABLE_ADMIN=true mimic              # no admin API at all
```

Whatever is left reserved is logged at startup, and a freed route is a normal mock path from that moment on. See [Environment Variables](/reference/environment/) for the full list of variables and [Admin API](/reference/admin-api/) for what the admin endpoints return.
