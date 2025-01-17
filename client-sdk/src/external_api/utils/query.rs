use intmax2_interfaces::api::error::ServerError;
use reqwest::{
    header::{HeaderMap, HeaderValue, AUTHORIZATION},
    Response,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::retry::with_retry;

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
    #[serde(default)]
    message: Option<String>,
}

pub async fn post_request<B: Serialize, R: DeserializeOwned>(
    base_url: &str,
    endpoint: &str,
    body: &B,
    bearer_token: Option<String>,
) -> Result<R, ServerError> {
    let url = format!("{}{}", base_url, endpoint);

    let mut headers = HeaderMap::new();
    if let Some(token) = bearer_token {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", token))
                .map_err(|e| ServerError::SerializeError(format!("Failed to set header: {}", e)))?,
        );
    }
    let client = reqwest::Client::new();
    let response = with_retry(|| async {
        client
            .post(&url)
            .headers(headers.clone())
            .json(body)
            .send()
            .await
    })
    .await
    .map_err(|e| ServerError::NetworkError(e.to_string()))?;
    let body_str = serde_json::to_string(body)
        .map_err(|e| ServerError::SerializeError(format!("Failed to serialize body: {}", e)))?;
    handle_response(response, &url, &Some(body_str)).await
}

pub async fn get_request<Q, R>(
    base_url: &str,
    endpoint: &str,
    query: Option<Q>,
    bearer_token: Option<String>,
) -> Result<R, ServerError>
where
    Q: Serialize,
    R: DeserializeOwned,
{
    let mut url = format!("{}{}", base_url, endpoint);
    let query_str = query
        .as_ref()
        .map(|q| {
            serde_qs::to_string(&q).map_err(|e| {
                ServerError::SerializeError(format!("Failed to serialize query: {}", e))
            })
        })
        .transpose()?;
    if query_str.is_some() {
        url = format!("{}?{}", url, query_str.as_ref().unwrap());
    }
    let client = reqwest::Client::new();
    let mut headers = HeaderMap::new();
    if let Some(token) = bearer_token {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", token))
                .map_err(|e| ServerError::SerializeError(format!("Failed to set header: {}", e)))?,
        );
    }
    let response = with_retry(|| async { client.get(&url).headers(headers.clone()).send().await })
        .await
        .map_err(|e| ServerError::NetworkError(e.to_string()))?;

    handle_response(response, &url, &query_str).await
}

async fn handle_response<R: DeserializeOwned>(
    response: Response,
    url: &str,
    request_str: &Option<String>,
) -> Result<R, ServerError> {
    let status = response.status();
    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Failed to read error response".to_string());
        let error_message = match serde_json::from_str::<ErrorResponse>(&error_text) {
            Ok(error_resp) => error_resp.message.unwrap_or_else(|| error_resp.error),
            Err(_) => error_text,
        };
        let abr_request = request_str
            .as_ref()
            .map(|s| s.chars().take(500).collect::<String>())
            .unwrap_or_else(|| "".to_string());
        return Err(ServerError::ServerError(
            status.into(),
            error_message,
            url.to_string(),
            abr_request,
        ));
    }
    response
        .json::<R>()
        .await
        .map_err(|e| ServerError::DeserializationError(e.to_string()))
}
