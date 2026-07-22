---
title: Path Parameters
description: Match a whole family of URLs with a single mock using :id or {id} segments.
---

A literal `path` matches exactly one URL. That's fine for `/users` or `/health`, but it gets impractical fast for REST-style resources — mocking `GET /users/1`, `GET /users/2`, and `GET /users/3` individually would mean a separate file per id.

Use a **named path parameter** instead, and a single mock covers every value in that segment.

## Basic shape

Both `:id` (Express-style) and `{id}` (OpenAPI-style) syntax are supported — pick whichever you're more used to:

```json
{
  "method": "GET",
  "path": "/users/:id",
  "status": 200,
  "response": { "id": "{{path.id}}", "name": "Mock User" }
}
```

```bash
curl http://localhost:8080/users/42
# { "id": "42", "name": "Mock User" }

curl http://localhost:8080/users/anything-else
# { "id": "anything-else", "name": "Mock User" }
```

The captured value is available in the response via [templating](/dynamic-responses/templating/) as `{{path.id}}`.

## Multiple parameters and nested resources

Add as many parameter segments as you need:

```json
{
  "method": "DELETE",
  "path": "/orgs/{org}/repos/{repo}",
  "status": 204,
  "response": null
}
```

```bash
curl -X DELETE http://localhost:8080/orgs/acme/repos/widgets
# 204 No Content
```

```json
{
  "method": "GET",
  "path": "/orders/:id/items/:itemId",
  "status": 200,
  "response": {
    "order_id": "{{path.id}}",
    "item_id": "{{path.itemId}}"
  }
}
```

Mix `:name` and `{name}` styles in the same path if you want — Mimic doesn't care, as long as each segment is unambiguous.

## An exact path always wins

If you define both an exact mock and a pattern mock for the same route, the exact one wins whenever it applies:

`mocks/get_user_42.json`:

```json
{
  "method": "GET",
  "path": "/users/42",
  "status": 200,
  "response": { "id": 42, "name": "Special Cased User", "vip": true }
}
```

`mocks/get_user_by_id.json`:

```json
{
  "method": "GET",
  "path": "/users/:id",
  "status": 200,
  "response": { "id": "{{path.id}}", "name": "Mock User" }
}
```

```bash
curl http://localhost:8080/users/42
# { "id": 42, "name": "Special Cased User", "vip": true }  <- exact mock

curl http://localhost:8080/users/7
# { "id": "7", "name": "Mock User" }  <- falls through to the pattern
```

This is the same specificity-wins idea as [match priority](/matching/priority/) for query params and headers: an exact `path` match scores **1000+**, a pattern match scores **900+** (100 lower), so the more specific mock is always preferred when both are eligible.

## Path parameters and stateful sequences

If a pattern mock also has a [`sequence`](/dynamic-responses/sequences/), the sequence advances on **every request that matches the pattern**, regardless of which concrete value was requested — it's one counter per mock definition, not one per id:

```json
{
  "method": "GET",
  "path": "/items/:id",
  "status": 200,
  "response": { "ok": true },
  "sequence": [
    { "status": 503, "response": { "error": "unavailable" } },
    { "status": 200, "response": { "ok": true }, "repeat": true }
  ]
}
```

```bash
curl http://localhost:8080/items/1   # 503 (step 0)
curl http://localhost:8080/items/2   # 200 (step 1 — a *different* id, same counter)
curl http://localhost:8080/items/99  # 200 (step 1 repeats)
```

If you need independent sequences per id, use separate exact mocks instead of a pattern.

## Performance

Exact path lookups are O(1) and completely unaffected by how many pattern mocks you have. Pattern matching is only attempted as a fallback when no exact mock matched, so mocks without path parameters see no performance change at all.

## Common gotchas

- **Parameter segments match anything except `/`.** `:id` in `/users/:id` won't match `/users/1/profile` — that needs its own segment, e.g. `/users/:id/profile`.
- **Parameter names can contain hyphens.** `:org-id` works fine — Mimic doesn't rely on Rust's regex named-group syntax, which would otherwise reject the hyphen.
- **No trailing catch-all.** There's no `/files/*rest` style wildcard — every segment after the last parameter still has to match exactly.
- **`{{path.id}}` needs the parameter to exist.** If you reference `{{path.slug}}` in the response but the mock's `path` never captures a `slug` segment, it silently renders as an empty string — see [Response Templating](/dynamic-responses/templating/) for the exact rules.
