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
use std::{collections::HashMap, convert::Infallible, io::Cursor, path::PathBuf, time::Duration};
use tracing::{debug, error, info};

// A custom guard that holds the entire Request and passes it along.
struct MyRequestGuard<'r> {
    request: &'r Request<'r>,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for MyRequestGuard<'r> {
    type Error = Infallible;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let converted: &'r Request<'r> = unsafe {
            std::mem::transmute::<&'r Request<'_>, &'r Request<'r>>(req)
        };
        Outcome::Success(MyRequestGuard { request: converted })
    }
}

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
    fn respond_to(self, _: &'r Request<'_>) -> rocket::response::Result<'static> {
        let mut response = Response::build();
        response.status(self.status);
        
        response.raw_header("Content-Length", self.body.len().to_string());
        
        if let Some(ct) = ContentType::parse_flexible(&self.content_type) {
            response.header(ct);
        }

        for (name, value) in self.headers {
            if name.to_lowercase() != "content-length" {
                response.header(Header::new(name, value));
            }
        }

        response.sized_body(self.body.len(), Cursor::new(self.body));
        response.ok()
    }
}

#[get("/<path..>?<params..>")]
async fn get_request(
    path: PathBuf,
    params: HashMap<String, String>,
    state: &State<AppState>,
    guard: MyRequestGuard<'_>,
) -> Result<ProxyResponse, ErrorResponse> {
    handle_request(Method::Get, path, Some(params), None, state, guard.request)
        .await
        .map_err(ErrorResponse)
}

#[post("/<path..>?<params..>", data = "<data>")]
async fn post_request(
    path: PathBuf,
    params: HashMap<String, String>,
    data: Data<'_>,
    state: &State<AppState>,
    guard: MyRequestGuard<'_>,
) -> Result<ProxyResponse, ErrorResponse> {
    handle_request(Method::Post, path, Some(params), Some(data), state, guard.request)
        .await
        .map_err(ErrorResponse)
}

#[put("/<path..>?<params..>", data = "<data>")]
async fn put_request(
    path: PathBuf,
    params: HashMap<String, String>,
    data: Data<'_>,
    state: &State<AppState>,
    guard: MyRequestGuard<'_>,
) -> Result<ProxyResponse, ErrorResponse> {
    handle_request(Method::Put, path, Some(params), Some(data), state, guard.request)
        .await
        .map_err(ErrorResponse)
}

#[delete("/<path..>?<params..>")]
async fn delete_request(
    path: PathBuf,
    params: HashMap<String, String>,
    state: &State<AppState>,
    guard: MyRequestGuard<'_>,
) -> Result<ProxyResponse, ErrorResponse> {
    handle_request(Method::Delete, path, Some(params), None, state, guard.request)
        .await
        .map_err(ErrorResponse)
}

async fn handle_request(
    method: Method,
    path: PathBuf,
    query_params: Option<HashMap<String, String>>,
    data: Option<Data<'_>>,
    state: &State<AppState>,
    req: &Request<'_>,
) -> Result<ProxyResponse> {
    let path_str = path.to_string_lossy();
    
    let mut url = format!("https://www.roblox.com/{}", path_str);
    
    if let Some(params) = query_params {
        if !params.is_empty() {
            info!("Query parameters: {:?}", params);
            let query_string: String = params
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&query_string);
        }
    }
    // info!("Incoming request method: {:?}", method);
    // info!("Incoming request path: {:?}", path);
    // info!("Incoming request headers:");
    // for header in req.headers().iter() {
    //     info!("  {}: {}", header.name(), header.value());
    // }
    info!("Full URL: {}", url);

    let mut request_builder = match method {
        Method::Get => state.client.get(&url),
        Method::Post => state.client.post(&url),
        Method::Put => state.client.put(&url),
        Method::Delete => state.client.delete(&url),
        _ => return Err(anyhow!("Unsupported method")),
    };

    request_builder = request_builder
        .header("Accept", "application/json")
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .header("Referer", "https://www.roblox.com")
        .header("Origin", "https://www.roblox.com");

    for header in req.headers().iter() {
        let name_lower = header.name().to_string().to_lowercase();
        if !["host", "connection", "content-length", "transfer-encoding", "user-agent", "roblox-id"].contains(&name_lower.as_str()) {
            debug!("Forwarding header: {} = {}", header.name(), header.value());
            request_builder = request_builder.header(header.name().as_str(), header.value());
        }
    }

    if let Some(data) = data {
        let body_bytes = data
            .open(5_i32.mebibytes())
            .into_bytes()
            .await
            .context("Failed to read request body")?;
        
        debug!("Request body size: {} bytes", body_bytes.len());
        request_builder = request_builder.body(body_bytes.to_vec());
    }

    info!("Sending request to Roblox API...");
    let response = request_builder
        .send()
        .await
        .context("Failed to send request")?;

    let status = response.status();
    info!("Received response status: {}", status);

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|val| val.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    let response_headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            if let Ok(val_str) = value.to_str() {
                let name_lower = name.to_string().to_lowercase();
                if !["transfer-encoding", "connection"].contains(&name_lower.as_str()) {
                    Some((name.to_string(), val_str.to_string()))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();

    let body = response.bytes().await.context("Failed to read response body")?;
    info!("Response body size: {} bytes", body.len());

    // if let Ok(json_str) = String::from_utf8(body.to_vec()) {
    //     info!("Response body: {}", json_str);
    // }

    Ok(ProxyResponse {
        status: Status::from_code(status.as_u16()).unwrap_or(Status::InternalServerError),
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
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
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
