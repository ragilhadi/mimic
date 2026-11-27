---
title: Runtime Mock Management
description: Create, edit, toggle, and persist mocks through the admin API.
---

Create a mock immediately with the admin API:

```sh
curl -X POST localhost:8080/admin/mocks \
  -H 'Content-Type: application/json' \
  -d '{"method":"GET","path":"/ready","status":200,"response":{"ready":true}}'
```

`POST /admin/mocks` returns the runtime mock's `id`, also shown by
`GET /admin/mocks`. Use `PUT /admin/mocks/{id}` with a full replacement config,
`DELETE /admin/mocks/{id}` to remove it, or
`POST /admin/mocks/{id}/disable` and `POST /admin/mocks/{id}/enable` to toggle it.
The dashboard's **Mocks** tab also exposes the toggles for runtime mocks.

Runtime mocks survive hot reload. Add `?persist=true` on creation to write the
mock under `mocks/_runtime/`. Edits and toggles update the file, and deleting
the mock removes it. After restart, the file is loaded as a file-backed mock
and is edited on disk after that point. Relative response fixtures submitted through the
API resolve from the mocks root. A directory is needed for persistence.

`MIMIC_ADMIN_TOKEN` protects these API calls. Set `MIMIC_ADMIN_READONLY=true`
to omit the runtime mock write routes and dashboard toggles. Set
`MIMIC_DISABLE_ADMIN=true` to remove the entire admin API.

See the [Admin API reference](/reference/admin-api/#runtime-mock-endpoints) for endpoint methods.
