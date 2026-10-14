# Kitae's Role: The Embeddable Web Engine for Rust

> The companion doc [CPP_ROLE.md](CPP_ROLE.md) covers *how C++ fits in*.
> This one covers *what Kitae is* at the ecosystem level — the product
> identity behind the Swift frontend and the embedder API.

## The idea in one line

**QuickJS was the little embeddable JS engine; Kitae is the little
embeddable web engine — in Rust, without the JS.**

This is a comparison of *role in an ecosystem*, not of function. V8 and
QuickJS are embeddable engines the JavaScript world builds applications on.
Kitae is the embeddable engine the Rust world (and anything that speaks a C
ABI) builds web-capable applications on. Concepts are the same; the product
is not.

## The role mapping

| | QuickJS / V8 | Kitae |
|---|---|---|
| **What it embeds** | JS execution capability | Web rendering capability |
| **What an embedder gets** | "my app can run scripts" | "my app can display web pages" |
| **Host API** | C API (`JS_NewRuntime`, `JS_Eval`) / C++ Isolate API | Rust API natively + C ABI facade for everyone else |
| **Famous embedder** | Node.js embeds V8 | The Kitae Swift app is the first embedder — Kitae's "Node moment" |
| **Selling point** | Small, embeddable, standards-complete | Small, embeddable, JS-free, memory-safe |

The pattern is proven in both directions: our old (deleted) C17 codebase
*embedded* QuickJS through its C API; Kitae now *plays that role* — other
programs embed *it*.

## Why this is stronger than a JS engine, not weaker

V8 alone cannot show anyone a web page. Embed V8 and you get script
execution only — no networking, no HTML parsing, no layout, no pixels
(Node.js spent years building up from that; Chrome needs Blink *beside* V8
just to render). Kitae is the complete stack out of the box:

```
fetch → parse → style → layout → display list
```

So in ecosystem terms: **V8 gives apps a language; Kitae gives apps the
web.** And the JS-free design is not a gap — it is the identity, the same
way QuickJS's identity was "tiny." Kitae's identity is: *embeddable web
rendering with zero script-execution attack surface, in a memory-safe
language, small enough for any app.* An e-reader, documentation viewer, RSS
client, mail app, or kiosk can embed it and ship no V8, no Chromium, no
CVE-factory.

## The empty slot in the Rust ecosystem

If a Rust developer wants web content in their app today, the options are:

1. **CEF (Chromium Embedded Framework)** — huge, C++, drags a JS engine in
   whether they want it or not.
2. **Platform WebKit bindings** — macOS-only.
3. **Servo** — Rust, but a heavy framework, notoriously hard to embed as a
   plain library.

There is **no small, pure-Rust, embeddable web-rendering engine**. That slot
is open. Kitae is aimed at it: roughly "what WebKit would be if you deleted
the JS engine and kept the reading."

## Scope of "web support" (be clear-eyed)

Apps embedding Kitae get **static web support**: text, structure, links,
and (later) images — not React apps, not scripted logins, not web apps.
That is the deal Kitae makes, and most documents and a large portion of the
web live within it. A downstream embedder who insists on scripting could
embed a JS engine themselves (e.g., Boa, pure Rust) — outside Kitae,
against its philosophy, and their choice, not ours.

## What it takes to actually be that

Kitae is currently an app with clean internal crates. To become an
embeddable engine it needs a **public embedder API** — the same facade the
Swift migration (step 1 in [CPP_ROLE.md](CPP_ROLE.md)) already requires:

```rust
// what an embedder should one day write
let mut engine = Engine::builder().viewport(800, 600).build();
engine.navigate("https://example.com");
let frame = engine.render();            // DisplayList to paint anywhere
for ev in engine.poll_events() { /* links, scroll, navigation */ }
```

Plus `crate-type = ["staticlib"]` and a C ABI facade for non-Rust
embedders. **One piece of work, two goals:** building the Swift frontend
forces the embedder API to exist, and the Swift app becomes the living
proof that an outside frontend can drive the engine — exactly as Node.js
proved V8 could live outside a browser.
