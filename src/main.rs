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
        
        ContentType::parse_flexible(&self.content_type)
            .map(|ct| response.header(ct));

        for (name, value) in self.headers {
            response.header(Header::new(name, value));
        }

        response.ok()
    }
}

#[get("/<path..>")]
async fn get_request(
    path: PathBuf,
    state: &State<AppState>,
) -> Result<ProxyResponse, status::Custom<String>> {
    handle_request(Method::Get, path, None, state).await
}

#[post("/<path..>", data = "<data>")]
async fn post_request(
    path: PathBuf,
    data: Data<'_>,
    state: &State<AppState>,
) -> Result<ProxyResponse, status::Custom<String>> {
    handle_request(Method::Post, path, Some(data), state).await
}

#[put("/<path..>", data = "<data>")]
async fn put_request(
    path: PathBuf,
    data: Data<'_>,
    state: &State<AppState>,
) -> Result<ProxyResponse, status::Custom<String>> {
    handle_request(Method::Put, path, Some(data), state).await
}

#[delete("/<path..>")]
async fn delete_request(
    path: PathBuf,
    state: &State<AppState>,
) -> Result<ProxyResponse, status::Custom<String>> {
    handle_request(Method::Delete, path, None, state).await
}

async fn handle_request(
    method: Method,
    path: PathBuf,
    data: Option<Data<'_>>,
    state: &State<AppState>,
) -> Result<ProxyResponse, status::Custom<String>> {
    let path_str = path.to_string_lossy();
    let mut url = format!("https://www.roblox.com/{}", path_str);
    
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

    // Store headers for forwarding
    let mut response_headers = Vec::new();
    let headers = response.headers().clone();
    for (name, value) in headers.iter() {
        if let Ok(value_str) = value.to_str() {
            if !["connection", "transfer-encoding"].contains(&name.as_str()) {
                response_headers.push((name.as_str().to_string(), value_str.to_string()));
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