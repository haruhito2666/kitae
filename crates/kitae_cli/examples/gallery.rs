//! The "Built with Kitae" examples gallery — a kitae_live site whose every
//! card is a working example of the library.
//!
//! ```text
//! cargo run -p kitae_cli --example gallery
//! # → http://127.0.0.1:8789/ — open in Kitae
//! ```

use std::rc::Rc;

use kitae_live::{Route, Server};
use kitae_ui::prelude::*;

fn counter_card() -> View {
    let count = use_signal(|| 0u32);
    view! {
        div(class: "card") {
            h3 { "Signals + events" }
            p { "Click the button: the handler runs on the server, your page
                 receives the smallest possible patch." }
            button(on:click = move |_| count.update(|c| *c += 1)) { "Count: {count}" }
        }
    }
}

fn list_card() -> View {
    let items = use_signal(|| vec!["view! builds trees".to_string()]);
    let draft = use_signal(String::new);
    view! {
        div(class: "card") {
            h3 { "Two-way bindings" }
            p { "Typing fires input events server-side; the list is server
                 state — it survives disconnects and reconnects." }
            input(id: "gallery-item", placeholder: "add…", value: draft.get(),
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
}

fn validation_card() -> View {
    let email = use_signal(String::new);
    let verdict = use_signal(String::new);
    view! {
        div(class: "card") {
            h3 { "Forms + validation" }
            p { "Validation is plain Rust on the server — the error text is
                 just another signal." }
            input(id: "gallery-email", placeholder: "you@example.com", value: email.get(),
                  on:input = move |e| {
                      let v = e.value.unwrap_or_default();
                      email.update(|s| *s = v);
                  })
            button(on:click = move |_| {
                let e = email.get();
                let ok = e.contains('@') && e.contains('.');
                verdict.update(|v| {
                    *v = if ok { "Looks good.".into() } else { "That is not an email.".into() };
                });
            }) { "Check" }
            p { "{verdict}" }
        }
    }
}

fn resource_card() -> View {
    let fact = use_resource(|| {
        // Stand-in for any slow fetch: runs on a background thread; the
        // page patches the moment it lands.
        std::thread::sleep(std::time::Duration::from_millis(600));
        "Computed on a worker thread: 6 * 7 = 42".to_string()
    });
    let got = fact.get().unwrap_or_else(|| "computing…".to_string());
    view! {
        div(class: "card") {
            h3 { "use_resource" }
            p { "Data fetching off the render thread; the page updates when
                 the result lands." }
            p { "{got}" }
        }
    }
}

fn main() {
    let mut server = Server::new("127.0.0.1:8789").expect("bind 8789");
    server.mount(Route::new(
        "Built with Kitae",
        Rc::new(|| {
            view! {
                div {
                    h1 { "Built with Kitae" }
                    p { "Every card below is a live component — no script is
                         shipped to your browser, ever." }
                    {counter_card()}
                    {list_card()}
                    {validation_card()}
                    {resource_card()}
                }
            }
        }),
    ));
    println!("gallery: http://127.0.0.1:8789/ — open in Kitae");
    server.serve();
}
