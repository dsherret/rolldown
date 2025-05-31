use deno_cache_dir::file_fetcher::{HeaderMap, SendError, SendResponse};
use deno_error::JsErrorBox;
use deno_npm_cache::{NpmCacheHttpClientBytesResponse, NpmCacheHttpClientResponse};
use reqwest::{StatusCode, header};
use url::Url;

#[derive(Debug, Default, Clone)]
pub struct RolldownHttpClient {
  client: reqwest::Client,
}

#[async_trait::async_trait(?Send)]
impl deno_cache_dir::file_fetcher::HttpClient for RolldownHttpClient {
  async fn send_no_follow(&self, url: &Url, headers: HeaderMap) -> Result<SendResponse, SendError> {
    eprintln!("Downloading: {}", url);
    let response = self
      .client
      .get(url.clone())
      .headers(headers)
      .send()
      .await
      .map_err(|err| SendError::Failed(Box::new(err)))?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
      Ok(SendResponse::NotModified)
    } else if response.status().is_redirection() {
      // todo: how to not clone?
      let headers = response.headers().clone();
      Ok(SendResponse::Redirect(headers))
    } else if response.status() == reqwest::StatusCode::NOT_FOUND {
      Err(SendError::NotFound)
    } else if response.status().is_server_error() {
      Err(SendError::StatusCode(response.status()))
    } else {
      // todo: how to not clone?
      let headers = response.headers().clone();
      let bytes = response.bytes().await.map_err(|err| SendError::Failed(Box::new(err)))?;
      Ok(SendResponse::Success(headers, bytes.into()))
    }
  }
}

#[async_trait::async_trait(?Send)]
impl deno_npm_cache::NpmCacheHttpClient for RolldownHttpClient {
  // todo: implement retrying
  async fn download_with_retries_on_any_tokio_runtime(
    &self,
    url: Url,
    maybe_auth: Option<String>,
    maybe_etag: Option<String>,
  ) -> Result<NpmCacheHttpClientResponse, deno_npm_cache::DownloadError> {
    eprintln!("Downloading: {}", url);
    let mut headers = HeaderMap::new();
    if let Some(auth) = maybe_auth {
      headers.append(header::AUTHORIZATION, header::HeaderValue::try_from(auth).unwrap());
    }
    if let Some(etag) = maybe_etag {
      headers.append(header::IF_NONE_MATCH, header::HeaderValue::try_from(etag).unwrap());
    }
    let response = self.client.get(url.clone()).headers(headers).send().await.map_err(|err| {
      deno_npm_cache::DownloadError {
        status_code: err.status().map(|s| s.as_u16()),
        error: JsErrorBox::generic(err.to_string()),
      }
    })?;
    if response.status() == StatusCode::NOT_FOUND {
      Ok(NpmCacheHttpClientResponse::NotFound)
    } else if response.status() == StatusCode::NOT_MODIFIED {
      Ok(NpmCacheHttpClientResponse::NotModified)
    } else if response.status().is_success() {
      let headers = response.headers().clone(); // todo: do not clone here
      let body = response.bytes().await.map_err(|err| deno_npm_cache::DownloadError {
        status_code: err.status().map(|s| s.as_u16()),
        error: JsErrorBox::generic(err.to_string()),
      })?;
      Ok(NpmCacheHttpClientResponse::Bytes(NpmCacheHttpClientBytesResponse {
        etag: headers.get(header::ETAG).and_then(|e| e.to_str().map(|t| t.to_string()).ok()),
        bytes: body.into(),
      }))
    } else {
      Err(deno_npm_cache::DownloadError {
        status_code: Some(response.status().as_u16()),
        error: JsErrorBox::generic(response.status().canonical_reason().unwrap_or("unknown error")),
      })
    }
  }
}
