---
title: Query Parameter Matching
description: Match requests based on URL query string parameters.
---

Use `query_params` to return different responses based on what's in the URL query string. This is useful for paginated endpoints, search endpoints, and anything where the path is the same but the behavior changes based on a `?key=value`.

## Basic shape

```json
{
  "method": "GET",
  "path": "/search",
  "status": 200,
  "query_params": {
    "params": {
      "q": "test",
      "page": "1"
    },
    "strict": false
  },
  "response": {
    "results": [{ "id": 1, "title": "Test Result" }]
  }
}
```

`params` is the object of expected query parameters. `strict` controls whether extra params in the request are allowed.

## Matching modes

Each value inside `params` can be one of four matcher types:

### Exact match

```json
{ "page": "1" }
```

The param must be present and equal to that string. Note that query values are always strings — write `"1"`, not `1`.

### Regex match

```json
{ "page": { "regex": "^[0-9]+$" } }
```

The param must be present and match the regex. Useful for validating numeric IDs, UUIDs, or restricting to an enum-like set:

```json
{ "limit": { "regex": "^(10|20|50|100)$" } }
```

### Any value

```json
{ "status": { "any": null } }
```

The param must be present, but any value is accepted. Use this when you only care that the client *sent* the parameter, not what they sent.

### Strict mode

```json
{
  "query_params": {
    "params": { "q": "test" },
    "strict": true
  }
}
```

With `strict: true`, the request must contain *exactly* the listed params — no extras. With `strict: false` (the default), extras are ignored.

## Worked example: pagination

`mocks/list_users_page_1.json`:

```json
{
  "method": "GET",
  "path": "/users",
  "status": 200,
  "query_params": {
    "params": { "page": "1" }
  },
  "response": {
    "users": [{ "id": 1 }, { "id": 2 }],
    "page": 1,
    "total_pages": 3
  }
}
```

`mocks/list_users_page_2.json`:

```json
{
  "method": "GET",
  "path": "/users",
  "status": 200,
  "query_params": {
    "params": { "page": "2" }
  },
  "response": {
    "users": [{ "id": 3 }, { "id": 4 }],
    "page": 2,
    "total_pages": 3
  }
}
```

Now:

```bash
curl http://localhost:8080/users?page=1
# Returns users 1 and 2

curl http://localhost:8080/users?page=2
# Returns users 3 and 4

curl http://localhost:8080/users?page=99
# 404 - no matching mock
```

If you want a fallback for unknown pages, add a third mock with no `query_params` matcher (so it matches anything) and a lower-priority response. Mimic's [priority scoring](/matching/priority/) will pick the more specific mock when one matches.

## Matching multiple params

All listed params must match (this is an AND, not an OR):

```json
{
  "query_params": {
    "params": {
      "q": { "any": null },
      "page": { "regex": "^[0-9]+$" },
      "sort": { "regex": "^(asc|desc)$" }
    }
  }
}
```

This matches any request to the configured path that has all three params, with `page` numeric and `sort` being either `asc` or `desc`.

## Repeated keys

`?tag=a&tag=b` and `?ids=1&ids=2` are the standard encoding for a list-valued parameter — what `URLSearchParams`, axios, Go's `url.Values`, and an OpenAPI `explode: true` array parameter all emit. Two more matcher forms handle them:

```json
{
  "query_params": {
    "params": {
      "tag": { "contains": "b" },
      "ids": { "list": ["1", "2"] }
    }
  }
}
```

- `"param": {"contains": "value"}` matches if *any* occurrence of the key equals `value`.
- `"param": {"list": ["a", "b"]}` matches only the exact, ordered set of values the key carried — `?tag=a&tag=b` matches `["a", "b"]`, not `["b", "a"]` or `["a"]` alone.
- `"param": "value"` (plain exact match) and [`{{query.param}}`](/dynamic-responses/templating/) both still read only the parameter's **first** value — a mock written before repeated keys were matchable keeps meaning exactly what it always did.
- `GET /admin/requests` and the [dashboard](/guides/admin-dashboard/) report every value a repeated key carried, in the order sent, as a list — `{"tag": ["a", "b"]}` — rather than silently keeping only the last one.
- A repeated field in an `application/x-www-form-urlencoded` body (`opt=a&opt=b`) gets the same first-value treatment as `{{query.param}}` for `body` matchers and templating.
- **Strict mode counts a repeated key once.** `strict: true` rejects extra *parameter names*, not extra values — sending `?tag=a&tag=b` against a mock that declares `tag` doesn't fail strict mode just because the key appeared twice.

## Common gotchas

- **Values are strings.** A request to `/users?page=1` has a query param `page` with value `"1"`, not `1`. Always quote.
- **URL encoding.** Mimic compares decoded values. `?q=hello%20world` matches `"q": "hello world"`.
- **Order doesn't matter.** `?a=1&b=2` and `?b=2&a=1` are equivalent for matching purposes.
- **Repeated params without `contains`/`list`** — a plain exact matcher only ever sees the first value sent. See [Repeated Keys](#repeated-keys) above.
