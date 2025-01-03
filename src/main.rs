#[macro_use]
extern crate rocket;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use rocket::{
    data::ToByteUnit,
    http::{ContentType, Header, Method, Status},
    request::{FromRequest, Outcome},
    response::{self, Response},
    routes, Data, Request, State,
};
use std::{convert::Infallible, io::Cursor, path::PathBuf, time::Duration};
use tracing::{debug, error, info};

// A custom guard that holds the entire Request and passes it along.
// We rely on `transmute` in from_request to convert &'r Request<'_> to &'r Request<'r>.
struct MyRequestGuard<'r> {
    request: &'r Request<'r>,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for MyRequestGuard<'r> {
    type Error = Infallible;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        // SAFETY: We know that Rocket won't invalidate 'req' while it’s in scope,
        // so transmuting &'r Request<'_> to &'r Request<'r> is acceptable in this narrow case.
        let converted: &'r Request<'r> = unsafe {
            std::mem::transmute::<&'r Request<'_>, &'r Request<'r>>(req)
        };
        Outcome::Success(MyRequestGuard { request: converted })
    }
}

// Custom error type implementing Responder for consistent error handling.
pub struct ErrorResponse(anyhow::Error);

impl From<anyhow::Error> for ErrorResponse {
    fn from(err: anyhow::Error) -> Self {
        ErrorResponse(err)
    }
}

impl<'r> response::Responder<'r, 'static> for ErrorResponse {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        error!("{:?}", self.0);

        Response::build()
            .status(Status::InternalServerError)
            .header(ContentType::Plain)
            .sized_body(None, Cursor::new("Internal Server Error"))
            .ok()
    }
}

// Stores our HTTP client in Rocket state
struct AppState {
    client: Client,
}

// Struct to capture the proxied response
struct ProxyResponse {
    status: Status,
    content_type: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl<'r> rocket::response::Responder<'r, 'static> for ProxyResponse {
    fn respond_to(self, _: &'r Request<'_>) -> rocket::response::Result<'static> {
        let mut response = Response::build();
        response.status(self.status);
        response.sized_body(self.body.len(), Cursor::new(self.body));

        if let Some(ct) = ContentType::parse_flexible(&self.content_type) {
            response.header(ct);
        }

        for (name, value) in self.headers {
            response.header(Header::new(name, value));
        }
        response.ok()
    }
}

// GET route
#[get("/<path..>?<query..>")]
async fn get_request(
    path: PathBuf,
    query: Option<String>,
    state: &State<AppState>,
    guard: MyRequestGuard<'_>,
) -> Result<ProxyResponse, ErrorResponse> {
    handle_request(Method::Get, path, query, None, state, guard.request)
        .await
        .map_err(ErrorResponse)
}

// POST route
#[post("/<path..>?<query..>", data = "<data>")]
async fn post_request(
    path: PathBuf,
    query: Option<String>,
    data: Data<'_>,
    state: &State<AppState>,
    guard: MyRequestGuard<'_>,
) -> Result<ProxyResponse, ErrorResponse> {
    handle_request(Method::Post, path, query, Some(data), state, guard.request)
        .await
        .map_err(ErrorResponse)
}

// PUT route
#[put("/<path..>?<query..>", data = "<data>")]
async fn put_request(
    path: PathBuf,
    query: Option<String>,
    data: Data<'_>,
    state: &State<AppState>,
    guard: MyRequestGuard<'_>,
) -> Result<ProxyResponse, ErrorResponse> {
    handle_request(Method::Put, path, query, Some(data), state, guard.request)
        .await
        .map_err(ErrorResponse)
}

// DELETE route
#[delete("/<path..>?<query..>")]
async fn delete_request(
    path: PathBuf,
    query: Option<String>,
    state: &State<AppState>,
    guard: MyRequestGuard<'_>,
) -> Result<ProxyResponse, ErrorResponse> {
    handle_request(Method::Delete, path, query, None, state, guard.request)
        .await
        .map_err(ErrorResponse)
}

// Core proxy logic: build a request, forward it, and transform the result into a ProxyResponse.
async fn handle_request(
    method: Method,
    path: PathBuf,
    query: Option<String>,
    data: Option<Data<'_>>,
    state: &State<AppState>,
    req: &Request<'_>,
) -> Result<ProxyResponse> {
    let path_str = path.to_string_lossy();
    let mut url = format!("https://www.roblox.com/{}", path_str);

    // Improved query parameter handling
    if let Some(q) = query {
        debug!("Raw query string: {}", q);
        
        // Parse the query string properly
        let query_pairs: Vec<(String, String)> = form_urlencoded::parse(q.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        
        debug!("Parsed query parameters:");
        for (key, value) in &query_pairs {
            debug!("  {} = {}", key, value);
        }

        // Append query string to URL
        if !q.is_empty() {
            url = format!("{}?{}", url, q);
        }
    }

    info!("Proxying {:?} request to: {}", method, url);

    let mut request_builder = match method {
        Method::Get => state.client.get(&url),
        Method::Post => state.client.post(&url),
        Method::Put => state.client.put(&url),
        Method::Delete => state.client.delete(&url),
        _ => return Err(anyhow!("Unsupported method")),
    };

    // Forward headers while excluding problematic ones
    debug!("Forwarding headers:");
    let excluded_headers = [
        "host",
        "connection",
        "content-length",
        "x-frame-options", // Exclude X-Frame-Options to prevent warning
        "transfer-encoding",
    ];

    for header in req.headers().iter() {
        let name_lower = header.name().to_string().to_lowercase();
        if !excluded_headers.contains(&name_lower.as_str()) {
            debug!("  {}: {}", header.name(), header.value());
            request_builder = request_builder.header(header.name().as_str(), header.value());
        }
    }

    // Read and forward body if present
    if let Some(data) = data {
        let body_bytes = data
            .open(5_i32.mebibytes())
            .into_bytes()
            .await
            .context("Failed to read request body")?;

        request_builder = request_builder.body(body_bytes.to_vec());
    }

    // Timeout for external call
    let response = match tokio::time::timeout(Duration::from_secs(30), request_builder.send()).await {
        Ok(result) => result.context("Failed to send HTTP request")?,
        Err(_) => {
            return Err(anyhow!("Request to Roblox timed out"));
        }
    };

    let status_code = response.status().as_u16();
    let status = Status::from_code(status_code).unwrap_or(Status::InternalServerError);

    // Extract content-type
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|val| val.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    debug!("Response status code: {}", status_code);
    debug!("Response Content-Type: {}", content_type);

    // Collect headers, excluding problematic ones
    let mut response_headers = Vec::new();
    for (name, value) in response.headers().iter() {
        if let Ok(val_str) = value.to_str() {
            let name_lower = name.to_string().to_lowercase();
            if !excluded_headers.contains(&name_lower.as_str()) {
                response_headers.push((name.to_string(), val_str.to_string()));
            }
        }
    }

    let body = response.bytes().await.context("Failed to read response body")?;
    debug!("Response body length: {} bytes", body.len());

    // If it's JSON, we show a preview for debugging
    if content_type.contains("application/json") {
        debug!("JSON body preview: {}", String::from_utf8_lossy(&body));
    }

    Ok(ProxyResponse {
        status,
        content_type,
        body: body.to_vec(),
        headers: response_headers,
    })
}

// Shuttle integration
#[shuttle_runtime::main]
async fn main() -> shuttle_rocket::ShuttleRocket {
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
        .configure(
            rocket::Config::figment()
                .merge(("limits", rocket::data::Limits::new().limit("data-form", 5_i32.mebibytes()))),
        );

    Ok(rocket.into())
}