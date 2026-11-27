---
title: Stateful CRUD Collections
description: Mock a REST resource with in-memory data and configurable CRUD operations.
---

This mock file declares an in-memory collection at `/users`:

```json
{
  "method": "GET",
  "path": "/users",
  "collection": {
    "name": "users",
    "path": "/users",
    "id_field": "id",
    "seed": [{"id": 1, "name": "Ada"}],
    "operations": {
      "create": {"required_fields": ["name"], "status": 201}
    }
  }
}
```

A collection serves list and create at its base path and get, replace, patch,
and delete at `/{id}`. Create assigns an ID when omitted and returns `201`
with a `Location` header. A missing row returns `404`. Delete defaults to
`204`. Collection names should be unique. Rows survive hot reload and
`POST /admin/reset` restores the seed.

The optional `operations` keys are `list`, `get`, `create`, `replace`, `patch`,
and `delete`. Each can set a success `status` in the 2xx range and a fixed JSON
`response`. The three body-writing operations can also require fields by
name. A missing required field returns `400` before changing the collection.

| Operation | Request | Default status |
|---|---|---|
| `list` | `GET /users` | 200 |
| `get` | `GET /users/{id}` | 200 |
| `create` | `POST /users` | 201 |
| `replace` | `PUT /users/{id}` | 200 |
| `patch` | `PATCH /users/{id}` | 200 |
| `delete` | `DELETE /users/{id}` | 204 |

`id_field` defaults to `id`, and `seed` defaults to an empty array. Send a JSON object to create, replace, or patch a row. PUT replaces the row and PATCH updates its top-level fields. Both preserve the existing ID. Attempting to change it or creating a row with an existing ID returns `409`. Rows start from the seed after a process restart.

A collection declaration requires the top-level `method` and `path` fields. The `collection.path` field sets the CRUD base path. Omit top-level `status`, response bodies, sequences, and request matchers. Use `collection.operations` to customize success responses.
