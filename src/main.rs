#[macro_use]
extern crate rocket;

use reqwest::Client;
use rocket::data::ToByteUnit;
use rocket::{
    http::{Method, Status},
    response::status,
    routes, Data, Request, State,
};
use std::path::PathBuf;
use std::time::Duration;

// Define AppState struct at the top level
struct AppState {
    client: Client,
}

#[get("/<path..>")]
async fn get_request(
    path: PathBuf,
    state: &State<AppState>,
) -> Result<status::Custom<Vec<u8>>, status::Custom<String>> {
    handle_request(Method::Get, path, None, state).await
}

#[post("/<path..>", data = "<data>")]
async fn post_request(
    path: PathBuf,
    data: Data<'_>,
    state: &State<AppState>,
) -> Result<status::Custom<Vec<u8>>, status::Custom<String>> {
    handle_request(Method::Post, path, Some(data), state).await
}

#[put("/<path..>", data = "<data>")]
async fn put_request(
    path: PathBuf,
    data: Data<'_>,
    state: &State<AppState>,
) -> Result<status::Custom<Vec<u8>>, status::Custom<String>> {
    handle_request(Method::Put, path, Some(data), state).await
}

#[delete("/<path..>")]
async fn delete_request(
    path: PathBuf,
    state: &State<AppState>,
) -> Result<status::Custom<Vec<u8>>, status::Custom<String>> {
    handle_request(Method::Delete, path, None, state).await
}

// Shared request handling logic
async fn handle_request(
    method: Method,
    path: PathBuf,
    data: Option<Data<'_>>,
    state: &State<AppState>,
) -> Result<status::Custom<Vec<u8>>, status::Custom<String>> {
    let path_str = path.to_string_lossy();
    let roblox_url = format!("https://www.roblox.com/{}", path_str);

    // Build the request based on the incoming method
    let mut request_builder = match method {
        Method::Get => state.client.get(&roblox_url),
        Method::Post => state.client.post(&roblox_url),
        Method::Put => state.client.put(&roblox_url),
        Method::Delete => state.client.delete(&roblox_url),
        _ => {
            return Err(status::Custom(
                Status::MethodNotAllowed,
                "Method not supported".into(),
            ))
        }
    };

    // Handle request body for methods that support it
    if let Some(data) = data {
        let body_bytes = data
            .open(5_i32.mebibytes())
            .into_bytes()
            .await
            .map_err(|e| {
                status::Custom(
                    Status::InternalServerError,
                    format!("Failed to read request body: {}", e),
                )
            })?;

        request_builder = request_builder.body(body_bytes.to_vec());
    }

    // Send the request with timeout
    let response = tokio::time::timeout(Duration::from_secs(30), request_builder.send())
        .await
        .map_err(|_| status::Custom(Status::GatewayTimeout, "Request timed out".into()))?
        .map_err(|e| {
            status::Custom(
                Status::InternalServerError,
                format!("Request failed: {}", e),
            )
        })?;

    let status =
        Status::from_code(response.status().as_u16()).unwrap_or(Status::InternalServerError);

    let body = response.bytes().await.map_err(|e| {
        status::Custom(
            Status::InternalServerError,
            format!("Failed to read response body: {}", e),
        )
    })?;

    Ok(status::Custom(status, body.to_vec()))
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