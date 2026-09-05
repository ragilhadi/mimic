---
title: Header Matching
description: Match requests based on HTTP header values, prefixes, regex patterns, or forbidden headers.
---

Use `headers` to match against HTTP request headers. The most common use case is **authentication** — returning different responses based on the presence and shape of an `Authorization` header.

## Basic shape

```json
{
  "method": "GET",
  "path": "/api/protected",
  "status": 200,
  "headers": {
    "required": {
      "authorization": { "prefix": "Bearer " }
    },
    "forbidden": [],
    "strict": false
  },
  "response": {
    "data": "secret stuff"
  }
}
```

Three keys at the top of `headers`:

- `required` — headers that must be present and match the given pattern.
- `forbidden` — header names that must **not** be present.
- `strict` — if `true`, the request can have *only* the listed required headers.

## Matching modes for required headers

Each value in `required` can be one of five matcher types:

### Exact match

```json
{ "x-api-key": "abc123" }
```

The header value must equal exactly that string.

### Prefix match

```json
{ "authorization": { "prefix": "Bearer " } }
```

The header value must start with the given prefix. Ideal for token-style auth where you don't care about the token value, only its format.

### Contains

```json
{ "content-type": { "contains": "json" } }
```

The header value must contain the given substring. Matches `application/json`, `application/vnd.api+json`, etc.

### Regex match

```json
{ "x-api-key": { "regex": "^[A-Za-z0-9]{32}$" } }
```

The header value must match the regex. Use for strict format validation (e.g. exactly 32 alphanumeric characters).

### Any value

```json
{ "accept": { "any": null } }
```

The header must be present, but any value is accepted.

## Forbidden headers

Specify headers that must **not** be present:

```json
{
  "headers": {
    "required": { "authorization": { "prefix": "Bearer " } },
    "forbidden": ["x-debug", "x-internal"]
  }
}
```

This matches a request with a Bearer token but **without** any debug or internal headers — useful for simulating production behavior where internal headers would be stripped at a gateway.

## Strict mode's built-in ignore list

`"strict": true` rejects a request that carries a header the mock didn't declare in `required` — but a long list of headers never count as "extra", because a client sends them unconditionally and no mock is ever written to assert on them:

- `accept`, `accept-encoding`, `accept-language`, `cache-control`, `connection`, `content-length`, `dnt`, `host`, `origin`, `pragma`, `referer`, `upgrade-insecure-requests`, `user-agent`
- anything starting with `sec-` — `sec-fetch-mode`, `sec-fetch-site`, `sec-ch-ua`, `sec-ch-ua-platform`, and whatever browsers add to that family next, matched by prefix rather than enumerated by name

That's enough for `curl` and for a browser's `fetch()` alike — a strict mock that passes from `curl` no longer 404s from the browser it was actually written to test. A header your client sends that isn't on this list — say, a tracing header your gateway injects — can be added without a release:

```bash
MIMIC_STRICT_IGNORE_HEADERS=x-request-id,x-correlation-id mimic
```

Comma-separated, additive to the built-in list. Anything else undeclared, like `x-tenant: acme`, is still rejected — strict mode keeps meaning something.

## Header names are case-insensitive

HTTP header names are case-insensitive per the spec, and Mimic follows this rule. These are all equivalent:

```json
{ "authorization": "..." }
{ "Authorization": "..." }
{ "AUTHORIZATION": "..." }
```

Header *values*, however, are case-sensitive. `"Bearer abc"` and `"bearer abc"` are different.

## Worked example: protected endpoint with two outcomes

You can use two mocks to simulate "authorized" vs "unauthorized" responses for the same endpoint.

`mocks/protected_ok.json`:

```json
{
  "method": "GET",
  "path": "/api/account",
  "status": 200,
  "headers": {
    "required": { "authorization": { "prefix": "Bearer " } }
  },
  "response": {
    "id": 1,
    "email": "alice@example.com"
  }
}
```

`mocks/protected_unauthorized.json`:

```json
{
  "method": "GET",
  "path": "/api/account",
  "status": 401,
  "response": {
    "error": "unauthorized",
    "message": "Authorization header required"
  }
}
```

Now:

```bash
# Returns 200 with account data
curl -H "Authorization: Bearer xyz" http://localhost:8080/api/account

# Returns 401
curl http://localhost:8080/api/account
```

Mimic prefers the more specific mock (the one with the `headers` matcher) when both could match. See [Match Priority](/matching/priority/) for the scoring rules.

## Common gotchas

- **Header names are case-insensitive, values are not.** Don't lowercase a token value.
- **The `host` and `content-length` headers** are managed by the HTTP client and Mimic itself — matching against them rarely behaves the way you'd expect. Stick to application-level headers.
- **Bearer tokens with a trailing space.** A common mistake: `"prefix": "Bearer"` (without the trailing space) will match `"Bearertoken"` as well. Use `"prefix": "Bearer "` with the space.
