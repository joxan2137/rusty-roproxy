#[macro_use]
extern crate rocket;

use rocket::{
    http::{Method, Status, Header, ContentType},
    response::{status, Response},
    routes,
    State,
    Data,
};
use reqwest::Client;
use std::{time::Duration, path::PathBuf, io::Cursor};
use rocket::data::ToByteUnit;

// Define AppState struct at the top level
struct AppState {
    client: Client,
}

// Custom response struct to handle headers and content type
struct ProxyResponse {
    status: Status,
    content_type: String,
    body: Vec<u8>,
    headers: Vec<Header<'static>>,
}

impl<'r> rocket::response::Responder<'r, 'static> for ProxyResponse {
    fn respond_to(self, _: &'r rocket::Request<'_>) -> rocket::response::Result<'static> {
        let mut response = Response::build();
        response.sized_body(self.body.len(), Cursor::new(self.body));
        response.status(self.status);
        
        // Set content type
        if let Ok(ct) = ContentType::parse_flexible(&self.content_type) {
            response.header(ct);
        }

        // Add other headers
        for header in self.headers {
            response.header(header);
        }

        response.ok()
    }
}

#[get("/<path..>?<query..>")]
async fn get_request(
    path: PathBuf,
    query: Option<rocket::http::uri::Query<'_>>,
    headers: &rocket::http::HeaderMap<'_>,
    state: &State<AppState>,
) -> Result<ProxyResponse, status::Custom<String>> {
    handle_request(Method::Get, path, query, headers, None, state).await
}

#[post("/<path..>?<query..>", data = "<data>")]
async fn post_request(
    path: PathBuf,
    query: Option<rocket::http::uri::Query<'_>>,
    headers: &rocket::http::HeaderMap<'_>,
    data: Data<'_>,
    state: &State<AppState>,
) -> Result<ProxyResponse, status::Custom<String>> {
    handle_request(Method::Post, path, query, headers, Some(data), state).await
}

#[put("/<path..>?<query..>", data = "<data>")]
async fn put_request(
    path: PathBuf,
    query: Option<rocket::http::uri::Query<'_>>,
    headers: &rocket::http::HeaderMap<'_>,
    data: Data<'_>,
    state: &State<AppState>,
) -> Result<ProxyResponse, status::Custom<String>> {
    handle_request(Method::Put, path, query, headers, Some(data), state).await
}

#[delete("/<path..>?<query..>")]
async fn delete_request(
    path: PathBuf,
    query: Option<rocket::http::uri::Query<'_>>,
    headers: &rocket::http::HeaderMap<'_>,
    state: &State<AppState>,
) -> Result<ProxyResponse, status::Custom<String>> {
    handle_request(Method::Delete, path, query, headers, None, state).await
}

// Shared request handling logic
async fn handle_request(
    method: Method,
    path: PathBuf,
    query: Option<rocket::http::uri::Query<'_>>,
    headers: &rocket::http::HeaderMap<'_>,
    data: Option<Data<'_>>,
    state: &State<AppState>,
) -> Result<ProxyResponse, status::Custom<String>> {
    let path_str = path.to_string_lossy();
    let mut url = format!("https://www.roblox.com/{}", path_str);
    
    // Add query parameters if present
    if let Some(q) = query {
        url.push('?');
        url.push_str(q.as_str());
    }

    // Build the request based on the incoming method
    let mut request_builder = match method {
        Method::Get => state.client.get(&url),
        Method::Post => state.client.post(&url),
        Method::Put => state.client.put(&url),
        Method::Delete => state.client.delete(&url),
        _ => return Err(status::Custom(
            Status::MethodNotAllowed,
            "Method not supported".into()
        )),
    };

    // Forward headers except some specific ones we want to exclude
    for header in headers.iter() {
        if !["host", "connection", "content-length"].contains(&header.name().as_str().to_lowercase().as_str()) {
            request_builder = request_builder.header(header.name().as_str(), header.value());
        }
    }

    // Handle request body for methods that support it
    if let Some(data) = data {
        let body_bytes = data
            .open(5_i32.mebibytes())
            .into_bytes()
            .await
            .map_err(|e| status::Custom(
                Status::InternalServerError,
                format!("Failed to read request body: {}", e)
            ))?;

        request_builder = request_builder.body(body_bytes.to_vec());
    }

    // Send the request with timeout
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        request_builder.send()
    )
    .await
    .map_err(|_| status::Custom(
        Status::GatewayTimeout,
        "Request timed out".into()
    ))?
    .map_err(|e| status::Custom(
        Status::InternalServerError,
        format!("Request failed: {}", e)
    ))?;

    let status = Status::from_code(response.status().as_u16())
        .unwrap_or(Status::InternalServerError);

    // Get content type and other headers
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    // Convert response headers to Rocket headers
    let mut response_headers = Vec::new();
    for (name, value) in response.headers() {
        if let Ok(header_value) = value.to_str() {
            if !["connection", "transfer-encoding"].contains(&name.as_str()) {
                response_headers.push(Header::new(name.as_str(), header_value));
            }
        }
    }

    let body = response
        .bytes()
        .await
        .map_err(|e| status::Custom(
            Status::InternalServerError,
            format!("Failed to read response body: {}", e)
        ))?;

    Ok(ProxyResponse {
        status,
        content_type,
        body: body.to_vec(),
        headers: response_headers,
    })
}

#[shuttle_runtime::main]
async fn main() -> shuttle_rocket::ShuttleRocket {
    // Create a client with more detailed configuration
    let client = Client::builder()
        .pool_idle_timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(10)
        .timeout(Duration::from_secs(30))
        .user_agent("RobloxProxy/1.0")
        .build()
        .expect("Failed to create HTTP client");

    let state = AppState { client };
    
    let rocket = rocket::build()
        .mount("/", routes![
            get_request,
            post_request,
            put_request,
            delete_request
        ])
        .manage(state)
        .configure(rocket::Config::figment()
            .merge(("limits", rocket::data::Limits::new()
                .limit("data-form", 5_i32.mebibytes()))));

    Ok(rocket.into())
}