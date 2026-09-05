---
title: Admin API
description: Full reference for every /admin/* endpoint Mimic exposes.
---

All admin endpoints are read-only apart from the ones that say otherwise, and all return JSON — the [dashboard](/guides/admin-dashboard/) is just one client of them. They live under `MIMIC_ADMIN_PREFIX` (default `/admin`) and can be disabled entirely with `MIMIC_DISABLE_ADMIN=true`, or put behind a bearer token with `MIMIC_ADMIN_TOKEN` — see [Environment Variables](/reference/environment/).

```
GET    /admin/dashboard         # Web dashboard (Requests / Mocks / Sequences)
GET    /admin/requests          # List recorded requests (see filters below)
DELETE /admin/requests          # Clear recorded requests
GET    /admin/mocks             # List every loaded mock, with matchers and hit counts
GET    /admin/sequences         # Current step of every in-progress sequence
POST   /admin/sequences/reset   # Reset sequence counters (optional: ?path=/api/submit)
GET    /admin/scenario          # Which scenario tags are currently active
POST   /admin/scenario          # Replace the active tag set (body: {"tags": [...]})
```

## `GET /admin/requests`

| Query param | Meaning |
|-------------|---------|
| `path` | Substring of the request path |
| `method` | Exact method, case-insensitive |
| `status` | Exact code (`404`) or class (`4xx`, `5xx`) |
| `unmatched_only` | `true`/`1`/`yes` — only requests no mock served |
| `search` | Case-insensitive text search over body, headers and query params |
| `limit` | How many matches to return, applied *after* every filter above. Defaults to `50`; `limit=0` returns every match |
| `offset` | How many of the most recent matches to skip before taking `limit` — `offset=0` (the default) is the newest page |

```bash
# Every 4xx whose path mentions "user" and that nothing matched
curl "http://localhost:8080/admin/requests?path=user&status=4xx&unmatched_only=true"

# The 50 requests immediately before the most recent 50
curl "http://localhost:8080/admin/requests?limit=50&offset=50"
```

Each record carries the request, plus — when there is something to report — the response served and the match diagnosis:

```json
{
  "id": 12,
  "timestamp": "2026-08-07T10:15:04Z",
  "method": "GET",
  "path": "/users/42",
  "query_params": { "fields": ["id", "name"] },
  "headers": { "accept": "application/json", "authorization": "[REDACTED]" },
  "matched_mock": "GET:/users/42",
  "response_status": 200,
  "response_body": "{\"id\":42}",
  "response_headers": { "content-type": "application/json" },
  "match_score": 900,
  "path_params": { "id": "42" },
  "match_explanation": "matched mocks/get_users_id.json (score 900: method+path 1000, path pattern -100)"
}
```

Every field after `response_status` is additive and omitted when empty.

`query_params` values are a **list** of every occurrence that key had in the request, in order — `?fields=id&fields=name` is `{"fields": ["id", "name"]}`, and a key sent once is still a one-element list, `{"page": ["1"]}`. See [Repeated Query Keys](/matching/query-params/#repeated-keys).

**Pagination.** The top level of the response carries a page of `requests` plus enough to know there's more:

```json
{
  "count": 1000,
  "returned": 50,
  "limit": 50,
  "offset": 0,
  "requests": [ /* the 50 most recent matches */ ]
}
```

`count` is the total number of requests matching the filters. `requests` is a *page* of that total — ask for `limit=0` to get every match in one response, at the cost of a much larger payload on a busy server.

## `GET /admin/mocks`

```json
{
  "count": 12,
  "mocks": [
    {
      "key": "GET:/users/:id",
      "index": 0,
      "method": "GET",
      "path": "/users/:id",
      "status": 200,
      "source": "mocks/generated/get_users_id.json",
      "has_path_params": true,
      "matchers": { "query_params": false, "headers": true, "body": false },
      "delay_ms": null,
      "sequence_steps": null,
      "response_headers": 1,
      "consume_body": false,
      "hits": 4,
      "tags": [],
      "active": true,
      "config": { "method": "GET", "path": "/users/:id", "status": 200, "response": {} }
    }
  ]
}
```

`source` is the file the mock was loaded from, and is absent for mocks not read from disk. `config` is the full mock configuration. `hits` counts requests served by that specific mock, so two mocks sharing a path stay distinguishable. `tags` and `active` describe the mock's [scenario](/guides/scenarios/) membership — `active: false` means the mock is loaded but filtered out by the current scenario, and so unmatchable.

## `GET /admin/sequences`

```json
{
  "count": 1,
  "sequences": [
    {
      "key": "POST:/submit#0",
      "method": "POST",
      "path": "/submit",
      "step": 2,
      "total": 3,
      "source": "mocks/post_submit.json"
    }
  ]
}
```

`step` is how many calls the sequence has served — the index of the step the next request will get. A sequence appears only once it has served a request. See [Stateful Sequences](/dynamic-responses/sequences/).

## `POST /admin/sequences/reset`

Returns the number of counters that were reset:

```json
{ "reset": 2 }
```

## `GET /admin/scenario`

```json
{
  "active_tags": ["happy-path"],
  "filtering": true,
  "known_tags": ["error-scenario", "happy-path"],
  "matchable_mocks": 12,
  "total_mocks": 14
}
```

`known_tags` is every tag declared by a loaded mock. `filtering` is `false` (and `active_tags` empty) when no scenario filter is configured, i.e. every mock is matchable.

## `POST /admin/scenario`

Replaces the active tag set and returns the same body as the `GET`:

```bash
curl -X POST http://localhost:8080/admin/scenario -d '{"tags": ["error-scenario"]}'
```

The body is read as JSON regardless of `Content-Type`, so plain `curl -d` works. `{"tags": []}` clears the filter; a malformed body is a `400` and leaves the current scenario untouched. See [Scenario Tags](/guides/scenarios/).
