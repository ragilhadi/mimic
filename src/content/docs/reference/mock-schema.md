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
  "response_file": "string",
  "template": false,
  "consume_body": false,
  "query_params": { /* ... */ },
  "headers": { /* ... */ },
  "body": { /* ... */ },
  "response_headers": { /* ... */ },
  "delay_ms": 0,
  "sequence": [ /* ... */ ],
  "tags": [ /* ... */ ]
}
```

Mimic accepts this same shape as `.json`, `.yaml`, or `.yml` — see [Mock Files](/guides/mock-files/#yaml-mocks).

| Field          | Type    | Required | Description |
|----------------|---------|----------|-------------|
| `method`       | string  | yes      | HTTP method. Case-insensitive but conventionally uppercase: `GET`, `POST`, `PUT`, `DELETE`, `PATCH`, `HEAD`, `OPTIONS`. |
| `path`         | string  | yes      | URL path. Must start with `/`. May contain named parameters (`:id`, `{id}`) — see [Path Parameters](/matching/path-parameters/). No other wildcards are supported. |
| `status`       | number  | yes      | HTTP status code returned for matching requests, `100`–`599`. An out-of-range value can't be put on the wire — Mimic serves `200 OK` instead, warns at load time, and marks the mock `"servable": false` in `GET /admin/mocks`. |
| `response`     | any     | no       | The JSON value returned as the response body. Can be an object, array, string, number, boolean, or `null`. String values support [`{{ }}` templating](/dynamic-responses/templating/). Mutually exclusive with `response_file`. |
| `response_file`| string  | no       | Serve the body from a file next to the mock instead of `response`. See [File-Backed Responses](/dynamic-responses/response-file/). |
| `template`     | boolean | no       | If `true`, interpolate `{{ }}` templates inside a `response_file` body. Default: `false`. Ignored when `response_file` isn't set. |
| `consume_body` | boolean | no       | If `true`, Mimic reads and discards the request body before responding. Required for file uploads and large payloads. Default: `false`. |
| `query_params` | object  | no       | Match against query string parameters. |
| `headers`      | object  | no       | Match against request headers. |
| `body`         | object  | no       | Match against the request body. |
| `response_headers` | object | no   | Custom response headers (name → string value). See [Custom Response Headers](/dynamic-responses/response-headers/). |
| `delay_ms`     | number or object | no | Delay before responding: a fixed number of milliseconds, or `{ "min": number, "max": number }` for a random range. See [Response Delays](/dynamic-responses/delays/). |
| `sequence`     | array   | no       | A list of `{ status, response, delay_ms?, repeat? }` steps served one per request. See [Stateful Sequences](/dynamic-responses/sequences/). |
| `tags`         | string[] | no      | Scenario tags — the mock is only matchable when one of them is active, or always matchable if omitted. See [Scenario Tags](/guides/scenarios/). |

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

## `response_headers`

```json
{
  "response_headers": {
    "<header_name>": "<string value>"
  }
}
```

A flat map of header name to string value, applied to the response. Names are case-insensitive. `Content-Type: application/json` is added automatically unless these headers already set one. See [Custom Response Headers](/dynamic-responses/response-headers/).

## `delay_ms`

```json
{ "delay_ms": 2000 }
```

or a random range:

```json
{ "delay_ms": { "min": 100, "max": 3000 } }
```

| Form | Description |
|------|-------------|
| `number` | Fixed delay in milliseconds before the response is sent. |
| `{ "min": number, "max": number }` | A fresh random delay, uniformly sampled between `min` and `max` (inclusive), on every request. |

See [Response Delays](/dynamic-responses/delays/).

## `sequence`

```json
{
  "sequence": [
    { "status": 200, "response": { /* ... */ }, "delay_ms": 0, "repeat": false }
  ]
}
```

An array of steps served **one per request**, in order.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `status` | number | yes | HTTP status code for this step. |
| `response` | any | yes | Response body for this step. |
| `delay_ms` | number | no | Delay in milliseconds for this step only; overrides the mock-level `delay_ms`. |
| `repeat` | boolean | no | If `true`, this step is served for all subsequent calls. Default: `false`. |

If no step sets `repeat: true`, the last step repeats once the array is exhausted. An empty array falls back to the mock's top-level `status`/`response`. See [Stateful Sequences](/dynamic-responses/sequences/).

## Full example: response features

A mock that combines path parameters, templating, custom headers, a delay, and a sequence:

```json
{
  "method": "POST",
  "path": "/orders/:id/pay",
  "status": 200,
  "response": { "order_id": "{{path.id}}", "status": "paid" },
  "response_headers": {
    "X-Processed-By": "mimic"
  },
  "delay_ms": { "min": 50, "max": 300 },
  "sequence": [
    { "status": 503, "response": { "error": "payment gateway unavailable" } },
    {
      "status": 200,
      "response": { "order_id": "{{path.id}}", "status": "paid" },
      "repeat": true
    }
  ]
}
```

- `path` captures `:id`, which both sequence steps echo back via `{{path.id}}`.
- The first call to `/orders/<any id>/pay` returns `503`; every call after that returns `200` — and this holds across *different* order ids, since it's one sequence per mock, not per id.
- Every response (both steps) carries the `X-Processed-By: mimic` header and a random 50–300ms delay.

## Full example: request matching

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
