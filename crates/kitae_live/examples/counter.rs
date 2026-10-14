//! Run a Live counter site: `cargo run -p kitae_live --example counter`
//! then open http://127.0.0.1:8787/ in Kitae — clicks run the handler
//! server-side and the page patches over the WebSocket; any other browser
//! gets the same page, statically.

use std::rc::Rc;

use kitae_ui::prelude::*;

fn main() {
    let count = use_signal(|| 0u32);
    let render = move || {
        view! {
            div(style: "max-width: 30rem; margin: 4rem auto; font-family: sans-serif") {
                h1 { "Kitae Live counter" }
                p { "Clicks run the authored Rust handler on the server; the page
                     receives patch ops over the WebSocket. No script is shipped." }
                button(on:click = move |_| count.update(|c| *c += 1)) { "Count: {count}" }
            }
        }
    };
    let mut server = kitae_live::Server::new("127.0.0.1:8787").expect("bind 8787");
    server.mount(kitae_live::Route::new("Counter", Rc::new(render)));
    println!("serving http://127.0.0.1:8787/ — open it in Kitae");
    server.serve();
}
