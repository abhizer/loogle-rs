use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::post;
use axum::Json;
use axum::{routing::get, Router};
use color_eyre::eyre::Result;
use flume::Receiver;
use notify::Event;
use serde::Deserialize;
use serde_json::Value;
use std::net::TcpListener;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::model::{Model, QueryResult};

#[derive(Debug)]
pub struct AppState {
    model: Arc<Mutex<Model>>,
    rx: Receiver<Event>,
    search_history: Arc<Mutex<Vec<String>>>,
}

impl AppState {
    async fn new(mut model: Model, rx: Receiver<Event>) -> Self {
        rx.drain().for_each(|e| {
            model.update(e);
        });

        Self {
            model: Arc::new(Mutex::new(model)),
            search_history: Arc::new(Mutex::new(vec![])),
            rx,
        }
    }
}

impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            model: Arc::clone(&self.model),
            search_history: Arc::clone(&self.search_history),
            rx: self.rx.clone(),
        }
    }
}

#[derive(Deserialize)]
struct SearchQuery {
    query: String,
}

pub async fn init(listener: TcpListener, model: Model, rx: Receiver<Event>) -> Result<()> {
    let state = AppState::new(model, rx).await;

    let app = Router::new()
        .route("/", get(index))
        .route("/search", get(query))
        .route("/history", get(history))
        .route("/history/clear", post(clear_history))
        .with_state(state);

    axum::Server::from_tcp(listener)?
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

async fn query(
    Query(params): Query<SearchQuery>,
    State(s): State<AppState>,
) -> Result<(StatusCode, Json<QueryResult>), StatusCode> {
    if params.query.is_empty() {
        return Err(StatusCode::NO_CONTENT);
    }

    let val = {
        let model = &mut s.model.lock().await;

        let save = s.rx.drain().map(|e| model.update(e)).any(|v| v);
        if save {
            _ = model.save().await;
        }

        model.query(&params.query)
    };

    {
        let history = &mut s.search_history.lock().await;
        history.push(params.query.clone());
    }

    Ok((StatusCode::OK, Json(val)))
}

async fn history(State(s): State<AppState>) -> (StatusCode, Json<Value>) {
    let history = { s.search_history.lock().await.clone() };

    (
        StatusCode::OK,
        axum::Json(serde_json::to_value(history).unwrap()),
    )
}

async fn clear_history(State(s): State<AppState>) -> StatusCode {
    {
        s.search_history.lock().await.clear();
    }

    StatusCode::OK
}

async fn index() -> Html<&'static str> {
    let index_html = include_str!("../index.html");
    Html(index_html)
}
