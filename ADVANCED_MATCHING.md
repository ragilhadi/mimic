# Advanced Matching Features

Mimic now supports advanced request matching beyond just HTTP method and path. This allows you to create more sophisticated mock scenarios based on:

1. **Query Parameters** - Match based on URL query string
2. **Headers** - Match based on HTTP headers
3. **Request Body** - Match based on request payload

---

## Quick Start

### Query Parameter Matching

Match requests based on URL query parameters:

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
  "response": {"results": []}
}
```

**Matches:**
- `GET /search?q=test&page=1` ✅
- `GET /search?page=1&q=test` ✅ (order doesn't matter)
- `GET /search?q=test&page=1&extra=value` ✅ (extra params ignored when strict=false)

**Doesn't Match:**
- `GET /search?q=wrong&page=1` ❌
- `GET /search?q=test` ❌ (missing page)

---

### Header Matching

Match requests based on HTTP headers:

```json
{
  "method": "GET",
  "path": "/api/protected",
  "status": 200,
  "headers": {
    "required": {
      "authorization": "Bearer token123"
    }
  },
  "response": {"data": "secret"}
}
```

**Matches:**
- Request with `Authorization: Bearer token123` header ✅

**Doesn't Match:**
- Request with `Authorization: Bearer wrong` ❌
- Request without Authorization header ❌

---

### Body Matching

Match requests based on request body content:

```json
{
  "method": "POST",
  "path": "/api/login",
  "status": 200,
  "body": {
    "type": "json",
    "partial": {
      "username": "admin"
    }
  },
  "response": {"token": "abc123"}
}
```

**Matches:**
- Body: `{"username": "admin", "password": "secret"}` ✅
- Body: `{"username": "admin", "extra": "field"}` ✅

**Doesn't Match:**
- Body: `{"username": "user"}` ❌
- Body: `{"password": "secret"}` ❌

---

## Detailed Configuration

### Query Parameters

#### Structure

```json
{
  "query_params": {
    "params": {
      "param_name": "exact_value",
      "another_param": {"regex": "^[0-9]+$"},
      "optional_param": {"any": null}
    },
    "strict": false
  }
}
```

#### Value Types

| Type | Example | Description |
|------|---------|-------------|
| Exact | `"value"` | Must match exactly |
| Regex | `{"regex": "^[0-9]+$"}` | Must match regex pattern |
| Any | `{"any": null}` | Parameter must exist, any value |

#### Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `params` | object | `{}` | Key-value pairs to match |
| `strict` | boolean | `false` | If true, reject extra params |

---

### Headers

#### Structure

```json
{
  "headers": {
    "required": {
      "header-name": "exact_value",
      "authorization": {"prefix": "Bearer "},
      "x-api-key": {"any": null}
    },
    "forbidden": ["x-debug"],
    "strict": false
  }
}
```

#### Value Types

| Type | Example | Description |
|------|---------|-------------|
| Exact | `"value"` | Must match exactly |
| Regex | `{"regex": "^Bearer [A-Za-z0-9]+$"}` | Must match regex |
| Prefix | `{"prefix": "Bearer "}` | Must start with |
| Contains | `{"contains": "json"}` | Must contain substring |
| Any | `{"any": null}` | Header must exist, any value |

#### Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `required` | object | `{}` | Headers that must be present and match |
| `forbidden` | array | `[]` | Headers that must NOT be present |
| `strict` | boolean | `false` | If true, reject extra headers |

**Note:** Header names are case-insensitive (per HTTP spec).

---

### Body Matching

#### JSON Body

```json
{
  "body": {
    "type": "json",
    "exact": {"key": "value"},
    "partial": {"name": "Alice"},
    "strict": false
  }
}
```

| Option | Type | Description |
|--------|------|-------------|
| `exact` | object | Entire body must match exactly |
| `partial` | object | Specified fields must match, others ignored |
| `strict` | boolean | If true with partial, reject extra fields |

#### Text Body

```json
{
  "body": {
    "type": "text",
    "exact": "exact string",
    "contains": "substring",
    "regex": "^pattern$"
  }
}
```

| Option | Type | Description |
|--------|------|-------------|
| `exact` | string | Entire body must match exactly |
| `contains` | string | Body must contain substring |
| `regex` | string | Body must match regex pattern |

#### Form Body

```json
{
  "body": {
    "type": "form",
    "fields": {
      "username": "admin",
      "password": "secret"
    },
    "strict": false
  }
}
```

For `application/x-www-form-urlencoded` content.

#### Special Types

```json
{"body": {"type": "any"}}   // Match any body
{"body": {"type": "empty"}} // Match empty body only
```

---

## Combined Matching

You can combine all matching types for precise mock matching:

```json
{
  "method": "POST",
  "path": "/api/search",
  "status": 200,
  "query_params": {
    "params": {
      "type": "user"
    }
  },
  "headers": {
    "required": {
      "authorization": {"prefix": "Bearer "},
      "content-type": "application/json"
    }
  },
  "body": {
    "type": "json",
    "partial": {
      "query": "Alice"
    }
  },
  "response": {
    "results": [{"id": 1, "name": "Alice"}]
  }
}
```

This mock only matches when:
1. Method is POST
2. Path is /api/search
3. Query param `type=user` is present
4. Authorization header starts with "Bearer "
5. Content-Type is application/json
6. Body contains `{"query": "Alice"}`

---

## Match Priority

When multiple mocks could match a request, Mimic uses a scoring system:

1. **Base score**: Method + Path match (1000 points)
2. **Query params**: +100 points per matched param
3. **Headers**: +50 points per matched header
4. **Body**: +500 points if body matches

The mock with the highest score wins.

---

## Examples

### Authentication Scenarios

#### Valid Token

```json
{
  "method": "GET",
  "path": "/api/user",
  "status": 200,
  "headers": {
    "required": {
      "authorization": "Bearer valid_token"
    }
  },
  "response": {"id": 1, "name": "Alice"}
}
```

#### Invalid Token (401)

```json
{
  "method": "GET",
  "path": "/api/user",
  "status": 401,
  "headers": {
    "required": {
      "authorization": {"prefix": "Bearer invalid"}
    }
  },
  "response": {"error": "Unauthorized"}
}
```

### Pagination

```json
{
  "method": "GET",
  "path": "/api/users",
  "status": 200,
  "query_params": {
    "params": {
      "page": {"regex": "^[0-9]+$"},
      "limit": {"regex": "^(10|20|50)$"}
    }
  },
  "response": {
    "users": [],
    "pagination": {"page": 1, "limit": 10}
  }
}
```

### Login Validation

```json
{
  "method": "POST",
  "path": "/api/login",
  "status": 200,
  "body": {
    "type": "json",
    "partial": {
      "username": "admin",
      "password": "correct"
    }
  },
  "response": {"token": "abc123", "success": true}
}
```

```json
{
  "method": "POST",
  "path": "/api/login",
  "status": 401,
  "body": {
    "type": "json",
    "partial": {
      "username": "admin",
      "password": "wrong"
    }
  },
  "response": {"error": "Invalid credentials", "success": false}
}
```

---

## Backward Compatibility

All existing mocks continue to work without changes. The new matching fields are **optional**.

**Before (still works):**
```json
{
  "method": "GET",
  "path": "/users",
  "status": 200,
  "response": {"users": []}
}
```

**After (with advanced matching):**
```json
{
  "method": "GET",
  "path": "/users",
  "status": 200,
  "query_params": {"params": {"page": "1"}},
  "response": {"users": []}
}
```

---

## Testing Your Mocks

### Using curl

```bash
# Query params
curl "http://localhost:8080/search?q=test&page=1"

# Headers
curl -H "Authorization: Bearer token123" http://localhost:8080/api/protected

# JSON body
curl -X POST \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"secret"}' \
  http://localhost:8080/api/login

# Combined
curl -X POST \
  -H "Authorization: Bearer token" \
  -H "Content-Type: application/json" \
  -d '{"query":"Alice"}' \
  "http://localhost:8080/api/search?type=user"
```

### Debugging

If a mock doesn't match, check the response:

```json
{
  "error": "mock not found",
  "method": "POST",
  "path": "/api/login",
  "query_params": {},
  "headers_received": ["content-type", "host", "accept"]
}
```

This shows what the server received, helping you debug mismatches.

---

## Performance Notes

- Mocks without advanced matchers use fast path (simple lookup)
- Body matching requires consuming the request body
- Use `strict: false` for better matching flexibility

### Regex and path-template compilation

Every regex Mimic derives from a mock file — `query_params` regex, `headers`
regex, body `text.regex`, and `:id`/`{id}` path templates — is compiled **once
per distinct pattern string** for the lifetime of the process and reused from
then on. Hot reload does not invalidate the cache, because it is keyed by the
pattern string itself. Invalid patterns are reported once, not once per
request.

Measured on a release build (`cargo test --release -- --ignored --nocapture`,
see `mod benches` in `src/matcher.rs`); figures are per request:

| Operation | Recompiling each request | Compiled once |
|---|---|---|
| One header regex matcher (`^[A-Za-z0-9]{32}$`) | ~46 µs | ~0.16 µs |
| Exact-path request, 20 path-template routes loaded | ~6.0 ms | ~0.3 µs |
| Path-parameter request, 20 path-template routes loaded | ~6.0 ms | ~7 µs |

The exact-path row is the largest win and comes from two changes together: the
pattern scan is skipped entirely when the exact lookup already matched, and
templates that *are* scanned are no longer recompiled per request.

---

## Example Mock Files

See the `mocks/advanced/` directory for complete examples:

- `get_search_with_query.json` - Query parameter matching
- `get_protected_with_auth.json` - Header matching
- `post_login_with_body.json` - Body matching
- `post_search_combined.json` - Combined matching
- `get_users_pagination.json` - Regex pattern matching

---

## Summary

| Feature | Use Case | Example |
|---------|----------|---------|
| Query Params | Pagination, filtering | `?page=1&limit=10` |
| Headers | Auth, content negotiation | `Authorization: Bearer ...` |
| Body (JSON) | Login, create/update | `{"username": "..."}` |
| Body (Form) | Form submissions | `username=admin&password=...` |
| Combined | Complex scenarios | All of the above |

**All features are optional and backward compatible!**
