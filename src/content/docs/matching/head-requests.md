---
title: HEAD Requests
description: A HEAD is answered from the mock the matching GET would have used, with an empty body.
---

`HEAD` isn't an exotic method you'd think to mock — it's one clients issue on their own initiative: HTTP libraries and CDNs probe with it before a `GET`, health checkers and load balancers default to it, and `curl -I` is the reflex when you want to see the headers a mock returns. RFC 9110 defines `HEAD` as `GET` without the body, so whenever a `HEAD` matches no mock of its own, Mimic answers it from the mock `GET` would have matched:

```bash
# Only GET /users is registered...
curl -sI -o /dev/null -w '%{http_code}\n' http://localhost:8080/users  # 200
curl -s  -o /dev/null -w '%{http_code}\n' http://localhost:8080/users  # 200
```

## Rules

- **Automatic, no configuration** — and an explicit `HEAD` mock always wins, exactly the way an explicit `OPTIONS` mock wins over the [CORS](/guides/cors/) preflight gap-filler. If you're mocking a deliberately broken `HEAD`, it serves.
- **Empty body, the GET's headers.** Status, `response_headers`, `content-type` and any CORS headers are the `GET`'s. `Content-Length` reports the size of the body the `GET` would have sent, per RFC 9110, so a client sizing a download gets the right answer.
- **The GET mock's matchers apply as usual.** Query, header and body matchers are evaluated against the incoming `HEAD` request, so a `GET` mock that requires `?page=1` requires it of the `HEAD` too. The fallback also works for [pattern routes](/matching/path-parameters/): `HEAD /users/42` is answered by `GET /users/:id`.
- **A HEAD is a call.** Since the GET mock really did serve the request, its semantics follow: `delay_ms` applies, the hit count increments, and a [`sequence`](/dynamic-responses/sequences/) advances. A `HEAD` between two `GET`s on a 503-then-200 sequence consumes the 503 step — worth keeping in mind when a client probes with `HEAD` before its real request.
- **Composes with other fallbacks.** `HEAD /things/` reaches a mock registered at `GET /things` ([trailing-slash tolerance](/matching/trailing-slash/)), and a CORS preflight for `Access-Control-Request-Method: HEAD` is answered exactly when the actual `HEAD` would be.

## Visible in the request log

The fallback is not magic. The [request log](/guides/admin-dashboard/) records the request as a `HEAD`, under the path actually requested, with an explanation naming the GET mock that answered it:

```
matched mocks/users.json (score 1000: method+path 1000, HEAD answered from the GET mock at `GET:/users`)
```

The response body is stored as a descriptor (`<N bytes of … — body withheld for HEAD>`), since the client never received one.

## Opting out

Set `MIMIC_HEAD_FROM_GET=false` to restore the old behavior: a `HEAD` is answered only by an explicit `HEAD` mock, and gets a `404` otherwise. See [Environment Variables](/reference/environment/).
