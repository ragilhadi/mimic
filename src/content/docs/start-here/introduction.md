---
title: Introduction
description: What Mimic is, who it's for, and when to reach for it.
---

**Mimic** is a self-hosted HTTP mock server. You give it a folder of JSON files describing fake API endpoints, and it serves them over HTTP. That's the whole product.

It exists because most "real" backends are too slow, too unreliable, or simply don't exist yet when you need to build against them. Mimic gives you a stand-in that you fully control.

## What Mimic is good at

- **Frontend development against a missing backend.** Your team hasn't built the API yet, but the UI ticket is sitting on your board. Mock it with Mimic, ship the UI, swap in the real backend later — turn on [built-in CORS](/guides/cors/) and point your dev server straight at it.
- **Reproducing edge cases.** Forcing a real backend to return a `503`, a slow response, or a specific malformed payload is hard. With Mimic it's one line of JSON — see [Response Delays](/dynamic-responses/delays/) and [Stateful Sequences](/dynamic-responses/sequences/).
- **Third-party API simulation.** Stripe, GitHub, payment gateways — anything you don't want to actually call during tests. [Proxy & record-and-replay](/guides/proxy/) can even build the mock set for you from real traffic the first time you exercise it.
- **CI pipelines.** Mimic starts in under a second and uses ~1.66 MiB of memory, so it's cheap to spin up alongside your test suite. [Scenario tags](/guides/scenarios/) let one mocks directory serve both your happy-path suite and your error-path suite.
- **Demos and local environments.** Run a credible backend stand-in without provisioning anything, and inspect exactly what was served from the [admin dashboard](/guides/admin-dashboard/).
- **Bootstrapping mocks from an existing contract.** Already have an OpenAPI spec? [Generate a whole mocks directory from it](/guides/openapi-import/) instead of hand-writing every file.

## What Mimic is not

Mimic is intentionally focused. It is **not**:

- A full API gateway.
- A stateful database or a contract-testing tool.
- A load-testing target (though it's fast enough to use as one in a pinch).
- A transparent proxy for a whole API surface — [proxying](/guides/proxy/) is an opt-in fallback for requests nothing local matches, not the primary way to run Mimic.

If you need any of the above, Mimic is the wrong tool. If you need *"return this JSON when someone hits this path,"* Mimic is exactly the tool.

## How it works

The mental model is simple:

1. You write JSON files that describe HTTP endpoints.
2. You mount that folder into the Mimic container.
3. Mimic reads the files at startup and rebuilds the route table whenever they change.
4. When a request comes in, Mimic finds the best-matching mock and returns the response.

A minimal mock file looks like this:

```json
{
  "method": "GET",
  "path": "/users",
  "status": 200,
  "response": { "users": [] }
}
```

That's a complete, working mock. Drop it in your `mocks/` folder, point Mimic at it, and `GET /users` will return `{"users": []}` with a `200`.

## Where to go next

- New to Mimic? Continue to the [Quick Start](/start-here/quick-start/).
- Already comfortable with Docker? Skip to [Installation](/start-here/installation/) for all the install options.
- Want to see the full feature set? See [Advanced Matching](/matching/overview/) and [Dynamic Responses](/dynamic-responses/templating/).
- Just need a syntax reference? Jump to the [Mock File Schema](/reference/mock-schema/).
