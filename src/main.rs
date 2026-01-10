use axum::{Router, extract::State, http, response, routing};
use std::sync;

struct GlobalState {
    jinja: minijinja::Environment<'static>,
}

#[tokio::main]
async fn main() {
    // init template engine and add templates
    let mut jinja = minijinja::Environment::new();
    jinja
        .add_template("index", include_str!("./index.html"))
        .unwrap();

    let app_state = sync::Arc::new(GlobalState { jinja });

    let app = Router::new()
        .route("/", routing::get(handler_index))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();

    println!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn handler_index(
    State(state): State<sync::Arc<GlobalState>>,
) -> Result<response::Html<String>, http::StatusCode> {
    let template = state.jinja.get_template("index").unwrap();

    let rendered = template
        .render(minijinja::context! {})
        .unwrap();

    Ok(response::Html(rendered))
}
