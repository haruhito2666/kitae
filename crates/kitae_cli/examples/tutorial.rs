//! The Kitae tutorial — served as a kitae_live site, viewed in Kitae.
//!
//! ```text
//! cargo run -p kitae_cli --example tutorial
//! # → http://127.0.0.1:8788/ — open in Kitae
//! ```
//!
//! Each page teaches one idea and DEMONSTRATES it: the counter page is a
//! counter (click it), the state page live-updates, and every page is
//! complete static HTML for any other browser.

use std::rc::Rc;

use kitae_live::{Route, Server};
use kitae_ui::prelude::*;

fn welcome() -> View {
    view! {
        div {
            h1 { "Kitae in ten minutes" }
            p { "This site is a Kitae Live site: components are Rust functions,
                 state lives on the server, and your browser receives patch
                 operations over a WebSocket. No script is shipped — ever." }
        }
    }
}

fn intro_page() -> Route {
    Route::new(
        "Tutorial",
        Rc::new(|| {
            view! {
                div {
                    {welcome()}
                    h2 { "What you are looking at" }
                    p { "Open any other browser on this URL: you get the same
                         page, statically. Kitae additionally opens the Live
                         socket — that is the only difference." }
                    h2 { "The pieces" }
                    ul {
                        li { "kitae_ui — signals, components, the view! macro" }
                        li { "kitae_live — the server that mounts them" }
                        li { "Kitae — the browser that speaks the patch protocol" }
                    }
                    p { "Next: /counter — a component whose state you can change." }
                }
            }
        }),
    )
    .at("/")
}

fn counter_page() -> Route {
    let count = use_signal(|| 0u32);
    Route::new(
        "Counter",
        Rc::new(move || {
            view! {
                div {
                    h1 { "A counter is the whole story" }
                    p { "The button below runs a Rust closure ON THE SERVER when
                         you click it. The server diffs the new view and sends
                         the smallest set of patches that updates your page." }
                    button(on:click = move |_| count.update(|c| *c += 1)) {
                        "Clicked {count} times"
                    }
                    p { "One signal, one closure, zero script shipped." }
                    p { "Next: /state — where state lives and why it survives." }
                }
            }
        }),
    )
    .at("/counter")
}

fn state_page() -> Route {
    let items = use_signal(|| vec!["ships patches".to_string(), "keeps state".to_string()]);
    let draft = use_signal(String::new);
    Route::new(
        "State",
        Rc::new(move || {
            view! {
                div {
                    h1 { "State is server-side" }
                    p { "Type below — the input event runs a handler on the
                         server; the page is patched back. Disconnect your
                         socket (or restart the browser) and reconnect: the
                         list is still here, because the state never left
                         the server." }
                    input(id: "item", placeholder: "add an item…", value: draft.get(),
                          on:input = move |e| {
                              let v = e.value.unwrap_or_default();
                              draft.update(|d| *d = v);
                          })
                    button(on:click = move |_| {
                        let v = draft.get();
                        if !v.trim().is_empty() {
                            items.update(|l| l.push(v));
                            draft.update(|d| d.clear());
                        }
                    }) { "Add" }
                    ul {
                        for items.iter().enumerate() -> (i, it) {
                            li(key: i) { "{it}" }
                        }
                    }
                }
            }
        }),
    )
    .at("/state")
}

fn main() {
    let mut server = Server::new("127.0.0.1:8788").expect("bind 8788");
    server.mount(intro_page());
    server.mount(counter_page());
    server.mount(state_page());
    println!("tutorial: http://127.0.0.1:8788/ — open in Kitae");
    server.serve();
}
