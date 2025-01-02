#[macro_use]
extern crate rocket;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use rocket::data::ToByteUnit;
use rocket::{
    http::{ContentType, Header, Method, Status},
    response::{status, Response},
    routes, Data, Request, State,
};
use std::{io::Cursor, path::PathBuf, time::Duration};
use tracing::{debug, error, info};

struct AppState {
    client: Client,
}

struct ProxyResponse {
    status: Status,
    content_type: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl<'r> rocket::response::Responder<'r, 'static> for ProxyResponse {
    fn respond_to(self, _: &'r rocket::Request<'_>) -> rocket::response::Result<'static> {
        let mut response = Response::build();
        response.sized_body(self.body.len(), Cursor::new(self.body));
        response.status(self.status);

        ContentType::parse_flexible(&self.content_type).map(|ct| response.header(ct));

        for (name, value) in self.headers {
            response.header(Header::new(name, value));
        }

        response.ok()
    }
}

#[get("/<path..>?<query..>")]
async fn get_request(
    path: PathBuf,
    query: Option<String>,
    state: &State<AppState>,
    request: &Request<'_>,
) -> Result<ProxyResponse> {
    handle_request(Method::Get, path, query, None, state, request).await
}

#[post("/<path..>?<query..>", data = "<data>")]
async fn post_request(
    path: PathBuf,
    query: Option<String>,
    data: Data<'_>,
    state: &State<AppState>,
    request: &Request<'_>,
) -> Result<ProxyResponse> {
    handle_request(Method::Post, path, query, Some(data), state, request).await
}

#[put("/<path..>?<query..>", data = "<data>")]
async fn put_request(
    path: PathBuf,
    query: Option<String>,
    data: Data<'_>,
    state: &State<AppState>,
    request: &Request<'_>,
) -> Result<ProxyResponse> {
    handle_request(Method::Put, path, query, Some(data), state, request).await
}

#[delete("/<path..>?<query..>")]
async fn delete_request(
    path: PathBuf,
    query: Option<String>,
    state: &State<AppState>,
    request: &Request<'_>,
) -> Result<ProxyResponse> {
    handle_request(Method::Delete, path, query, None, state, request).await
}

async fn handle_request(
    method: Method,
    path: PathBuf,
    query: Option<String>,
    data: Option<Data<'_>>,
    state: &State<AppState>,
    request: &Request<'_>,
) -> Result<ProxyResponse> {
    let path_str = path.to_string_lossy();

    // Construct the URL with query parameters
    let url = format!("https://www.roblox.com/{}", path_str);

    info!("Proxying request to: {}", url);

    // Build the request based on the incoming method
    let mut request_builder = match method {
        Method::Get => state.client.get(&url),
        Method::Post => state.client.post(&url),
        Method::Put => state.client.put(&url),
        Method::Delete => state.client.delete(&url),
        _ => {
            return Err(anyhow!("Method not supported"));
        }
    };

    // Add query parameters if available
    if let Some(q) = query {
        debug!("Query parameters: {}", q);
        request_builder = request_builder.query(&[("q", q)]);
    }

    // Forward headers
    debug!("Forwarding headers:");
    for header in request.headers().iter() {
        if !["host", "connection", "content-length"]
            .contains(&header.name().as_str().to_lowercase().as_str())
        {
            debug!("  {}: {}", header.name(), header.value());
            request_builder = request_builder.header(header.name().as_str(), header.value());
        }
    }

    // Handle request body for methods that support it
    if let Some(data) = data {
        let body_bytes = data
            .open(5_i32.mebibytes())
            .into_bytes()
            .await
            .context("Failed to read request body")?;

        request_builder = request_builder.body(body_bytes.to_vec());
    }

    // Send the request with timeout
    let response = match tokio::time::timeout(Duration::from_secs(30), request_builder.send()).await
    {
        Ok(result) => result.context("Request failed")?,
        Err(_) => {
            return Err(anyhow!("Request timed out"));
        }
    };

    debug!("Response status: {}", response.status());
    debug!("Response headers: {:?}", response.headers());

    let status =
        Status::from_code(response.status().as_u16()).unwrap_or(Status::InternalServerError);

    // Get content type and other headers
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    debug!("Content-Type: {}", content_type);

    // Store headers for forwarding
    let mut response_headers = Vec::new();
    for (name, value) in response.headers().iter() {
        if let Ok(value_str) = value.to_str() {
            if !["connection", "transfer-encoding"].contains(&name.as_str()) {
                debug!("Forwarding header: {}: {}", name, value_str);
                response_headers.push((name.as_str().to_string(), value_str.to_string()));
            }
        }
    }

    let body = response.bytes().await.context("Failed to read response body")?;

    debug!("Response body length: {} bytes", body.len());
    if content_type.contains("application/json") {
        debug!("JSON Response: {}", String::from_utf8_lossy(&body));
    }

    Ok(ProxyResponse {
        status,
        content_type,
        body: body.to_vec(),
        headers: response_headers,
    })
}

#[shuttle_runtime::main]
async fn main() -> shuttle_rocket::ShuttleRocket {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let client = Client::builder()
        .pool_idle_timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(10)
        .timeout(Duration::from_secs(30))
        .user_agent("RobloxProxy/1.0")
        .build()
        .context("Failed to create HTTP client")?;

    let state = AppState { client };

    let rocket = rocket::build()
        .mount(
            "/",
            routes![get_request, post_request, put_request, delete_request],
        )
        .manage(state)
        .configure(rocket::Config::figment().merge((
            "limits",
            rocket::data::Limits::new().limit("data-form", 5_i32.mebibytes()),
        )));

    Ok(rocket.into())
}