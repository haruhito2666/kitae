# Future Feature: The Kitae Bridge

> Status: **future, browser-layer feature — not scheduled.** This doc
> records the design so the engine never accidentally closes the door on
> it. It belongs to the *browser* (Kitae.app and any embedder's product
> layer), never to the engine crates.
>
> Companions: [EMBEDDING_VISION.md](EMBEDDING_VISION.md) ·
> [UI_LIBRARY_PLAN.md](UI_LIBRARY_PLAN.md) · [CPP_ROLE.md](CPP_ROLE.md)

## The idea in one line

**A server-side execution proxy: run the scripted web on a headless host,
stream its behavior to Kitae as kitae_live patch frames — so old-web sites
work in Kitae while the engine, the app, and the user's machine stay
completely script-free.**

## Why it exists

Kitae renders the reader-shaped web natively and beautifully. But some of
the web is app-shaped: pages whose content changes through script. Kitae
will never execute that script — not in the engine (no JS engine, ever),
not in the browser process (that would undo the zero-script-attack-surface
identity). The Bridge is the only architecture that serves those pages
*without breaking that promise anywhere*:

- The JS engine lives on a **remote host we do not ship** — a third-party
  headless browser (Chromium/WebKit) is where the script runs.
- What crosses the network to Kitae is **kitae_live protocol v1** — the
  same `KTLV` patch/event frames a native kitae_live site uses.
- Kitae's engine cannot tell a bridged site from a native one. That is
  the point: **zero engine changes, ever.**

## Precedent

**Opera Mini (2005–2018)** rendered pages on Opera's servers and streamed
a compact binary to a thin client; presses and form input round-tripped
to the server. It served hundreds of millions of users on devices that
could never have run the live web. Its lesson and its ceiling: superb for
reading-shaped content, wrong for app-shaped content — exactly the trade
Kitae makes deliberately, with better typography and no pixel-scraping.

## Architecture

```
┌─────────────────────────── Kitae.app (browser layer) ─────────────┐
│  SiteScript = "bridge"  →  connect wss://bridge.example/session   │
│  Engine: unchanged. Receives Patch frames, emits Event frames.    │
└───────────────────────────────┬───────────────────────────────────┘
                                │ KTLV v1 (WebSocket)
┌───────────────────────────────┴───────────────────────────────────┐
│                     Kitae Bridge host (separate product)           │
│   headless full browser (3rd-party, ships its own JS engine)       │
│   observers: DOM mutations → Patch ops · input → Event dispatch    │
│   session store: cookies, auth, scroll, focus, history             │
└────────────────────────────────────────────────────────────────────┘
```

The mutation observer on the host translates *observed behavior* — not
source code — into patch ops. No language translation of JS ever happens;
that idea was evaluated and rejected (see "Rejected alternative").

## Where it lives

| Piece | Layer | Notes |
|---|---|---|
| Patch/event protocol | engine (already specced) | kitae_live v1; engine gains nothing new |
| Bridge session UI | **browser** | opt-in prompt, per-site "use Bridge" toggle, bridged-site badge |
| Session store | **browser** (syncs to host) | cookies/auth handled by the host, never by Kitae |
| Bridge host server | separate deployment | our product, our ops; or third-party/host-your-own |
| Script execution | host only | never in engine, never in app process |

The browser-side work is: a settings/badge surface, a WebSocket client,
and the policy that decides which URLs are offered the Bridge. That's it.

## Design rules

1. **Opt-in, per site.** Bridging is offered (never silent) — a chip in
   the address pill: "This site needs scripts. Open via Bridge?"
2. **Credentials stay on the host.** The Bridge host holds cookies/sessions;
   Kitae holds nothing. A hosted Bridge is a password manager with legs —
   self-hosting must be first-class.
3. **The patch-op enum remains the only mutation surface.** The Bridge
   proves the guarantee rather than weakening it: even the scripted web
   reaches Kitae as auditable patch ops.
4. **Graceful degradation.** If the Bridge is off, unreachable, or the
   site fails there, the engine's normal noscript/content-policy path
   renders the static skeleton. The Bridge is enhancement, not dependency.
5. **Never free, never hidden.** Running headless browsers costs real
   money; a hosted Bridge is a paid/rationed service, self-host is free.

## Rejected alternative: runtime code translation

"Translate the site's JS (or other languages) to Rust/native at runtime,
in the browser." Rejected on first principles:

- Translating a language doesn't translate its **semantics** — dynamic
  types, prototypes, closures, event loop, GC must all be preserved by
  the translator, so translator-plus-runtime is just a JS engine with
  extra steps: slower and buggier than just shipping one.
- The language was never the hard part; the **platform API surface** is
  (DOM, CSSOM, Events, Fetch, Storage, Timers — hundreds of APIs). "Run
  the old web" therefore means implementing the entire DOM spec in our
  browser layer — a browser inside the browser.
- TypeScript already compiles to JS; every other scripting road arrives
  at the same mountain.

Translation solves nothing; observation (Bridge) or replacement
(kitae_ui) solves everything.

## When to build it

Only when all of these are true — none are today:

1. kitae_live v1 is shipped and stable (the Bridge is a thin client of it).
2. Kitae has enough users that "the sites I need don't work" is a real
   support category.
3. A hosted/self-host Bridge product decision has been made and funded.

Until then, this doc is the whole commitment: **the engine already speaks
the protocol the Bridge will need.**
