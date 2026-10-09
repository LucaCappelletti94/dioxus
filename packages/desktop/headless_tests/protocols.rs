//! Every protocol handler kind answers a page fetch, and `prevent_default` keeps a form submit on the page.

use dioxus::prelude::*;
use dioxus_desktop::{
    DesktopContext, use_asset_handler,
    wry::http::{Response, StatusCode},
};

#[path = "./utils.rs"]
mod utils;

pub fn main() {
    #[cfg(not(windows))]
    utils::check_app_exits_with_cfg(app, config());
}

fn cors_response(body: &[u8]) -> Response<std::borrow::Cow<'static, [u8]>> {
    // WebKit only lets the page read a custom-scheme response cross-origin with this header
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/plain")
        .header("Access-Control-Allow-Origin", "*")
        .body(body.to_vec().into())
        .unwrap()
}

fn config() -> dioxus_desktop::Config {
    dioxus_desktop::Config::new()
        .with_custom_protocol("sync-protocol", |_id, _request| cors_response(b"sync-ok"))
        .with_asynchronous_custom_protocol("async-protocol", |_id, _request, responder| {
            responder.respond(cors_response(b"async-ok"));
        })
}

fn app() -> Element {
    let desktop_context: DesktopContext = consume_context();

    let mut results = use_signal(Vec::new);

    use_asset_handler("asset-protocol", |_request, responder| {
        responder.respond(cors_response(b"asset-ok"));
    });

    use_effect(move || {
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

            let mut values = Vec::new();
            values.push(fetch_text("sync-protocol://sync.txt").await);
            values.push(fetch_text("async-protocol://async.txt").await);
            values.push(fetch_text("dioxus://index.html/asset-protocol/asset.txt").await);
            values.push(trigger_form_submit().await);

            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            values.push(page_still_loaded().await);

            results.set(values);
        });
    });

    let values = results.read();
    if values.len() == 5 {
        check("sync custom protocol fetch", "sync-ok", &values[0]);
        check("async custom protocol fetch", "async-ok", &values[1]);
        check("asset handler fetch", "asset-ok", &values[2]);
        check("form submit fired", "submitted", &values[3]);
        check("submit default was prevented", "still-loaded", &values[4]);

        desktop_context.close();
    }

    rsx! {
        div { id: "protocol-test" }
        form {
            id: "prevent-form",
            action: "sync-protocol://submit-target/",
            onsubmit: move |ev| {
                ev.prevent_default();
            },
            button { r#type: "submit", value: "Submit" }
        }
    }
}

async fn fetch_text(url: &str) -> Result<String, dioxus::document::EvalError> {
    document::eval(&format!(
        r#"let res = await fetch('{url}');
        if (!res.ok) throw new Error('http status ' + res.status);
        return await res.text();"#
    ))
    .join()
    .await
}

async fn trigger_form_submit() -> Result<String, dioxus::document::EvalError> {
    document::eval(
        r#"let form = document.getElementById('prevent-form');
        if (!form) throw new Error('form not found');
        form.requestSubmit();
        return 'submitted';"#,
    )
    .join()
    .await
}

async fn page_still_loaded() -> Result<String, dioxus::document::EvalError> {
    document::eval(
        r#"if (!document.getElementById('prevent-form')) {
            throw new Error('form gone, the page navigated');
        }
        if (typeof window.interpreter === 'undefined') {
            throw new Error('interpreter gone, the page navigated');
        }
        return 'still-loaded';"#,
    )
    .join()
    .await
}

fn check(name: &str, expected: &str, value: &Result<String, dioxus::document::EvalError>) {
    match value {
        Ok(body) => assert_eq!(body, expected, "{name}"),
        Err(err) => panic!("{name} failed: {err:?}"),
    }
}
