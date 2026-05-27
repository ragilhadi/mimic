---
title: Mock File Schema
description: Full reference for every field in a Mimic mock file.
---

This page is a complete reference for the JSON structure of a Mimic mock file. For a friendlier introduction, start with the [Mock Files guide](/guides/mock-files/).

## Top-level fields

```json
{
  "method": "string",
  "path": "string",
  "status": 200,
  "response": "any",
  "consume_body": false,
  "query_params": { /* ... */ },
  "headers": { /* ... */ },
  "body": { /* ... */ }
}
```

| Field          | Type    | Required | Description |
|----------------|---------|----------|-------------|
| `method`       | string  | yes      | HTTP method. Case-insensitive but conventionally uppercase: `GET`, `POST`, `PUT`, `DELETE`, `PATCH`, `HEAD`, `OPTIONS`. |
| `path`         | string  | yes      | URL path. Must start with `/`. Wildcards are not supported. |
| `status`       | number  | yes      | HTTP status code returned for matching requests. |
| `response`     | any     | yes      | The JSON value returned as the response body. Can be an object, array, string, number, boolean, or `null`. |
| `consume_body` | boolean | no       | If `true`, Mimic reads and discards the request body before responding. Required for file uploads and large payloads. Default: `false`. |
| `query_params` | object  | no       | Match against query string parameters. |
| `headers`      | object  | no       | Match against request headers. |
| `body`         | object  | no       | Match against the request body. |

## `query_params`

```json
{
  "query_params": {
    "params": {
      "<param_name>": "<matcher>"
    },
    "strict": false
  }
}
```

| Field    | Type    | Required | Description |
|----------|---------|----------|-------------|
| `params` | object  | yes      | Map of parameter name to matcher value. |
| `strict` | boolean | no       | If `true`, the request must contain *only* the listed params. Default: `false`. |

### Matcher values

A matcher in `params` can be:

| Form                              | Matches when |
|-----------------------------------|--------------|
| `"value"` (string)                | Param is present and equal to `"value"`. |
| `{ "regex": "^pattern$" }`        | Param is present and matches the regex. |
| `{ "any": null }`                 | Param is present, value is anything. |

See [Query Parameter Matching](/matching/query-params/) for examples.

## `headers`

```json
{
  "headers": {
    "required": {
      "<header_name>": "<matcher>"
    },
    "forbidden": ["<header_name>"],
    "strict": false
  }
}
```

| Field       | Type     | Required | Description |
|-------------|----------|----------|-------------|
| `required`  | object   | no       | Headers that must be present and match. |
| `forbidden` | string[] | no       | Header names that must not be present. |
| `strict`    | boolean  | no       | If `true`, the request must contain *only* the listed required headers. Default: `false`. |

### Matcher values

| Form                              | Matches when |
|-----------------------------------|--------------|
| `"value"` (string)                | Header value equals `"value"` exactly. |
| `{ "prefix": "Bearer " }`         | Header value starts with the prefix. |
| `{ "contains": "json" }`          | Header value contains the substring. |
| `{ "regex": "^pattern$" }`        | Header value matches the regex. |
| `{ "any": null }`                 | Header is present, value is anything. |

Header names are compared case-insensitively. See [Header Matching](/matching/headers/) for examples.

## `body`

```json
{
  "body": {
    "type": "json" | "text" | "form",
    /* type-specific fields */
  }
}
```

### `type: "json"`

| Field     | Type    | Required | Description |
|-----------|---------|----------|-------------|
| `exact`   | any     | no       | Entire JSON body must equal this value. |
| `partial` | object  | no       | These fields must be present in the body. Extras are allowed unless `strict: true`. |
| `strict`  | boolean | no       | With `partial`, reject requests that have extra fields. |

Use exactly one of `exact` or `partial`.

### `type: "text"`

| Field      | Type   | Required | Description |
|------------|--------|----------|-------------|
| `exact`    | string | no       | Body must equal this string exactly. |
| `contains` | string | no       | Body must contain this substring. |
| `regex`    | string | no       | Body must match this regex. |

Use exactly one of `exact`, `contains`, or `regex`.

### `type: "form"`

| Field    | Type    | Required | Description |
|----------|---------|----------|-------------|
| `fields` | object  | yes      | Form fields that must be present with the given values. |
| `strict` | boolean | no       | If `true`, reject requests with extra fields. Default: `false`. |

See [Request Body Matching](/matching/body/) for examples.

## Full example

A mock that combines every matcher type:

```json
{
  "method": "POST",
  "path": "/api/search",
  "status": 200,
  "consume_body": true,
  "query_params": {
    "params": {
      "type": "user",
      "page": { "regex": "^[0-9]+$" }
    },
    "strict": false
  },
  "headers": {
    "required": {
      "authorization": { "prefix": "Bearer " },
      "content-type": { "contains": "json" }
    },
    "forbidden": ["x-debug"],
    "strict": false
  },
  "body": {
    "type": "json",
    "partial": {
      "query": "Alice"
    }
  },
  "response": {
    "results": [
      { "id": 1, "name": "Alice Johnson" }
    ],
    "total": 1
  }
}
```

This mock only matches a request that:

- Uses `POST` to `/api/search`
- Has `?type=user&page=<digits>` in the query string
- Has an `Authorization` header starting with `Bearer `
- Has a `Content-Type` header containing `json`
- Does **not** have an `x-debug` header
- Has a JSON body containing at least `{"query": "Alice"}`

See [Match Priority](/matching/priority/) for how this scores against other mocks for the same path.
