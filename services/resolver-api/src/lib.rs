use std::collections::HashMap;
use std::env;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::header::{
    ACCEPT, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, LOCATION, REFERER,
    USER_AGENT,
};
use axum::http::{HeaderName, HeaderValue, Method, Request as HttpRequest, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use resolver_core::{
    validate_download_url, validate_image_download_url, DouyinResolver, ResolveError,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{RwLock, Semaphore};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;
use uuid::Uuid;

const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8080";
const DEFAULT_BODY_LIMIT_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_IN_FLIGHT: usize = 8;
const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_DOWNLOAD_DIR: &str = "data/downloads";
const DEFAULT_PUBLIC_DOWNLOAD_BASE: &str = "/downloads";
const REQUEST_ID_HEADER: &str = "x-request-id";

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub bind_addr: SocketAddr,
    pub body_limit_bytes: usize,
    pub max_in_flight: usize,
    pub max_download_bytes: u64,
    pub download_dir: PathBuf,
    pub web_dir: PathBuf,
    pub public_download_base: String,
    cors_origins: CorsOrigins,
}

impl ApiConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_addr = env_value("QSY_API_BIND_ADDR", DEFAULT_BIND_ADDR)
            .parse::<SocketAddr>()
            .map_err(|error| ConfigError::new("QSY_API_BIND_ADDR", error))?;
        let body_limit_bytes = parse_bounded_env(
            "QSY_API_BODY_LIMIT_BYTES",
            DEFAULT_BODY_LIMIT_BYTES,
            1024,
            1024 * 1024,
        )?;
        let max_in_flight =
            parse_bounded_env("QSY_API_MAX_IN_FLIGHT", DEFAULT_MAX_IN_FLIGHT, 1, 128)?;
        let max_download_bytes = parse_bounded_env(
            "QSY_API_MAX_DOWNLOAD_BYTES",
            DEFAULT_MAX_DOWNLOAD_BYTES,
            1024 * 1024,
            20 * 1024 * 1024 * 1024,
        )?;
        let download_dir = PathBuf::from(env_value("QSY_API_DOWNLOAD_DIR", DEFAULT_DOWNLOAD_DIR));
        let web_dir = PathBuf::from(env_value("QSY_WEB_DIR", "dist"));
        let public_download_base = normalize_public_base(&env_value(
            "QSY_API_PUBLIC_DOWNLOAD_BASE",
            DEFAULT_PUBLIC_DOWNLOAD_BASE,
        ))?;
        let cors_origins = CorsOrigins::parse(&env_value("QSY_API_CORS_ORIGINS", "*"))?;

        Ok(Self {
            bind_addr,
            body_limit_bytes,
            max_in_flight,
            max_download_bytes,
            download_dir,
            web_dir,
            public_download_base,
            cors_origins,
        })
    }
}

fn env_value(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn parse_bounded_env<T>(name: &'static str, default: T, min: T, max: T) -> Result<T, ConfigError>
where
    T: Copy + fmt::Display + Ord + std::str::FromStr,
    T::Err: fmt::Display,
{
    let value = match env::var(name) {
        Ok(value) => value
            .parse::<T>()
            .map_err(|error| ConfigError::new(name, error))?,
        Err(env::VarError::NotPresent) => default,
        Err(error) => return Err(ConfigError::new(name, error)),
    };
    if value < min || value > max {
        return Err(ConfigError::message(
            name,
            format!("must be between {min} and {max}"),
        ));
    }
    Ok(value)
}

fn normalize_public_base(value: &str) -> Result<String, ConfigError> {
    let base = value.trim().trim_end_matches('/');
    if !base.starts_with('/') || base.contains("..") || base.contains(['?', '#']) {
        return Err(ConfigError::message(
            "QSY_API_PUBLIC_DOWNLOAD_BASE",
            "must be an absolute URL path without '..', query, or fragment",
        ));
    }
    Ok(base.to_string())
}

#[derive(Debug, Clone)]
enum CorsOrigins {
    Any,
    List(Vec<HeaderValue>),
}

impl CorsOrigins {
    fn parse(value: &str) -> Result<Self, ConfigError> {
        let values = value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if values == ["*"] {
            return Ok(Self::Any);
        }
        if values.is_empty() || values.contains(&"*") {
            return Err(ConfigError::message(
                "QSY_API_CORS_ORIGINS",
                "use '*' alone or provide a comma-separated origin list",
            ));
        }
        values
            .into_iter()
            .map(|value| {
                value.parse::<HeaderValue>().map_err(|error| {
                    ConfigError::message(
                        "QSY_API_CORS_ORIGINS",
                        format!("invalid origin {value:?}: {error}"),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self::List)
    }

    fn layer(&self) -> CorsLayer {
        let layer = CorsLayer::new()
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers([
                ACCEPT,
                CONTENT_TYPE,
                HeaderName::from_static(REQUEST_ID_HEADER),
            ])
            .expose_headers([HeaderName::from_static(REQUEST_ID_HEADER)])
            .max_age(Duration::from_secs(3600));
        match self {
            Self::Any => layer.allow_origin(Any),
            Self::List(origins) => layer.allow_origin(AllowOrigin::list(origins.clone())),
        }
    }
}

#[derive(Debug)]
pub struct ConfigError {
    variable: &'static str,
    message: String,
}

impl ConfigError {
    fn new(variable: &'static str, error: impl fmt::Display) -> Self {
        Self::message(variable, error.to_string())
    }

    fn message(variable: &'static str, message: impl Into<String>) -> Self {
        Self {
            variable,
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid {}: {}", self.variable, self.message)
    }
}

impl std::error::Error for ConfigError {}

type TaskStore = Arc<RwLock<HashMap<Uuid, TaskRecord>>>;

#[derive(Clone)]
struct AppState {
    resolver: DouyinResolver,
    client: reqwest::Client,
    tasks: TaskStore,
    slots: Arc<Semaphore>,
    download_dir: PathBuf,
    web_dir: PathBuf,
    public_download_base: String,
    max_download_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskFile {
    kind: &'static str,
    name: String,
    download_url: String,
    index: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskRecord {
    id: Uuid,
    status: TaskStatus,
    stage: String,
    progress: Option<u8>,
    title: Option<String>,
    video_id: Option<String>,
    download_url: Option<String>,
    files: Vec<TaskFile>,
    error: Option<String>,
}

impl TaskRecord {
    fn queued(id: Uuid) -> Self {
        Self {
            id,
            status: TaskStatus::Queued,
            stage: "等待后台处理".to_string(),
            progress: Some(0),
            title: None,
            video_id: None,
            download_url: None,
            files: Vec::new(),
            error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum TaskStatus {
    Queued,
    Processing,
    Completed,
    Failed,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateTaskRequest {
    share_text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateTaskResponse {
    task_id: Uuid,
    status_url: String,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    version: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiErrorBody {
    code: String,
    error: String,
    status: u16,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    body: ApiErrorBody,
}

impl ApiError {
    fn new(status: StatusCode, code: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            status,
            body: ApiErrorBody {
                code: code.into(),
                error: error.into(),
                status: status.as_u16(),
            },
        }
    }

    fn invalid_json(error: JsonRejection) -> Self {
        let status = error.status();
        let code = match status {
            StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
            _ => "invalid_json",
        };
        Self::new(status, code, error.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

pub async fn build_router(
    config: &ApiConfig,
    resolver: DouyinResolver,
) -> Result<Router, std::io::Error> {
    tokio::fs::create_dir_all(&config.download_dir).await?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(15 * 60))
        .user_agent(resolver_core::MOBILE_USER_AGENT)
        .build()
        .expect("valid HTTP client configuration");
    let state = AppState {
        resolver,
        client,
        tasks: Arc::new(RwLock::new(HashMap::new())),
        slots: Arc::new(Semaphore::new(config.max_in_flight)),
        download_dir: config.download_dir.clone(),
        web_dir: config.web_dir.clone(),
        public_download_base: config.public_download_base.clone(),
        max_download_bytes: config.max_download_bytes,
    };
    let request_id_header = HeaderName::from_static(REQUEST_ID_HEADER);
    let trace_layer = TraceLayer::new_for_http()
        .make_span_with(|request: &HttpRequest<Body>| tracing::info_span!("http_request", method = %request.method(), uri = %request.uri()))
        .on_response(DefaultOnResponse::new().level(Level::INFO));

    let download_route = format!("{}/{{file}}", config.public_download_base);
    Ok(Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/tasks", post(create_task))
        .route("/api/v1/tasks/{id}", get(get_task))
        .route(&download_route, get(serve_download))
        .route("/", get(serve_index))
        .route("/{*path}", get(serve_asset))
        .with_state(state)
        .layer(DefaultBodyLimit::max(config.body_limit_bytes))
        .layer(CatchPanicLayer::new())
        .layer(config.cors_origins.layer())
        .layer(PropagateRequestIdLayer::new(request_id_header.clone()))
        .layer(trace_layer)
        .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid)))
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn create_task(
    State(state): State<AppState>,
    payload: Result<Json<CreateTaskRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CreateTaskResponse>), ApiError> {
    let Json(request) = payload.map_err(ApiError::invalid_json)?;
    if request.share_text.trim().is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "missing_input",
            "请粘贴分享链接。",
        ));
    }
    if request.share_text.len() > 16 * 1024 {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "input_too_large",
            "分享内容过长。",
        ));
    }

    let id = Uuid::new_v4();
    state.tasks.write().await.insert(id, TaskRecord::queued(id));
    let worker_state = state.clone();
    tokio::spawn(async move {
        if let Err(error) = run_task(worker_state.clone(), id, request.share_text).await {
            tracing::warn!(task_id = %id, error = %error, "background task failed");
            update_task(&worker_state.tasks, id, |task| {
                task.status = TaskStatus::Failed;
                task.stage = "处理失败".to_string();
                task.progress = None;
                task.error = Some(public_error_message(&error));
            })
            .await;
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(CreateTaskResponse {
            task_id: id,
            status_url: format!("/api/v1/tasks/{id}"),
        }),
    ))
}

async fn get_task(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<Uuid>,
) -> Result<Json<TaskRecord>, ApiError> {
    state
        .tasks
        .read()
        .await
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "task_not_found",
                "任务不存在或服务已重启。",
            )
        })
}

async fn run_task(state: AppState, id: Uuid, share_text: String) -> Result<(), TaskError> {
    let _permit = state
        .slots
        .acquire()
        .await
        .map_err(|_| TaskError::Internal("任务队列已关闭。".to_string()))?;
    update_task(&state.tasks, id, |task| {
        task.status = TaskStatus::Processing;
        task.stage = "正在解析分享链接".to_string();
        task.progress = Some(8);
    })
    .await;

    let resolved = state.resolver.resolve_share_text(&share_text).await?;
    let video_id = resolved
        .video_id
        .clone()
        .unwrap_or_else(|| id.simple().to_string());
    let safe_video_id = video_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>();
    update_task(&state.tasks, id, |task| {
        task.title = resolved.title.clone();
        task.video_id = Some(video_id.clone());
    })
    .await;

    if resolved.images.is_empty() {
        let media_url = resolved.media_url.ok_or(TaskError::NoMedia)?;
        validate_download_url(&media_url)?;
        let file_name = format!(
            "qsy-{}-{}.mp4",
            if safe_video_id.is_empty() {
                "media"
            } else {
                &safe_video_id
            },
            &id.simple().to_string()[..8]
        );
        let final_path = state.download_dir.join(&file_name);
        let part_path = state
            .download_dir
            .join(format!("{file_name}.{}.part", id.simple()));
        update_task(&state.tasks, id, |task| {
            task.stage = "正在下载视频到服务器".to_string();
            task.progress = Some(15);
        })
        .await;
        download_media(&state, id, &media_url, &part_path).await?;
        tokio::fs::rename(&part_path, &final_path).await?;
        update_task(&state.tasks, id, |task| {
            task.status = TaskStatus::Completed;
            task.stage = "视频文件已准备好".to_string();
            task.progress = Some(100);
            task.download_url = Some(format!("{}/{}", state.public_download_base, file_name));
            task.files.push(TaskFile {
                kind: "video",
                name: file_name.clone(),
                download_url: format!("{}/{}", state.public_download_base, file_name),
                index: 0,
            });
        })
        .await;
        return Ok(());
    }

    let image_count = resolved.images.len();
    update_task(&state.tasks, id, |task| {
        task.stage = format!("正在下载 {image_count} 张图片到服务器");
        task.progress = Some(10);
    })
    .await;
    for image in resolved.images {
        let image_url = validate_image_download_url(&image.url)?;
        let extension = image_url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .and_then(|name| name.rsplit('.').next())
            .filter(|extension| {
                matches!(*extension, "jpg" | "jpeg" | "png" | "webp" | "gif" | "avif")
            })
            .unwrap_or("jpg");
        let file_name = format!(
            "qsy-{}-{}-{}.{}",
            safe_video_id,
            &id.simple().to_string()[..8],
            image.index + 1,
            extension
        );
        let path = state.download_dir.join(&file_name);
        let part_path = state
            .download_dir
            .join(format!("{file_name}.{}.part", id.simple()));
        if let Err(error) = download_image(&state, image_url, &part_path).await {
            let _ = tokio::fs::remove_file(&part_path).await;
            return Err(error);
        }
        tokio::fs::rename(&part_path, &path).await?;
        let download_url = format!("{}/{}", state.public_download_base, file_name);
        update_task(&state.tasks, id, |task| {
            task.files.push(TaskFile {
                kind: "image",
                name: file_name.clone(),
                download_url,
                index: image.index,
            });
            task.progress = Some((10 + ((image.index + 1) * 90 / image_count)) as u8);
        })
        .await;
    }
    update_task(&state.tasks, id, |task| {
        task.status = TaskStatus::Completed;
        task.stage = format!("{image_count} 张图片已准备好");
        task.progress = Some(100);
    })
    .await;
    Ok(())
}

async fn download_media(
    state: &AppState,
    id: Uuid,
    media_url: &str,
    part_path: &Path,
) -> Result<(), TaskError> {
    let response = match fetch_media_response(state, media_url).await {
        Ok(response) => response,
        Err(error) => {
            let _ = tokio::fs::remove_file(part_path).await;
            return Err(error);
        }
    };
    if let Some(length) = response.content_length() {
        if length > state.max_download_bytes {
            return Err(TaskError::TooLarge);
        }
    }
    let total = response.content_length();
    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(part_path).await?;
    let mut downloaded = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if downloaded > state.max_download_bytes {
            drop(file);
            let _ = tokio::fs::remove_file(part_path).await;
            return Err(TaskError::TooLarge);
        }
        file.write_all(&chunk).await?;
        let progress = total
            .filter(|value| *value > 0)
            .map(|value| (15 + downloaded.saturating_mul(84) / value).min(99) as u8);
        update_task(&state.tasks, id, |task| task.progress = progress).await;
    }
    file.flush().await?;
    Ok(())
}

async fn download_image(
    state: &AppState,
    initial_url: url::Url,
    path: &Path,
) -> Result<(), TaskError> {
    let mut current = initial_url;
    let mut response = None;
    for _ in 0..=5 {
        let current_response = state
            .client
            .get(current.clone())
            .header(USER_AGENT, resolver_core::MOBILE_USER_AGENT)
            .header(ACCEPT, "image/avif,image/webp,image/apng,image/*,*/*;q=0.5")
            .header(REFERER, "https://www.douyin.com/")
            .send()
            .await?;
        if current_response.status().is_redirection() {
            let location = current_response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| TaskError::Internal("图片重定向缺少地址。".to_string()))?;
            let next = current
                .join(location)
                .map_err(|_| TaskError::Internal("图片重定向地址无效。".to_string()))?;
            current = validate_image_download_url(next.as_str())?;
            continue;
        }
        if !current_response.status().is_success() {
            return Err(TaskError::DownloadHttp(current_response.status().as_u16()));
        }
        if current_response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                let value = value.to_ascii_lowercase();
                value.starts_with("text/") || value.starts_with("application/json")
            })
        {
            return Err(TaskError::InvalidContent);
        }
        response = Some(current_response);
        break;
    }
    let response =
        response.ok_or_else(|| TaskError::Internal("图片重定向次数过多。".to_string()))?;
    if response
        .content_length()
        .is_some_and(|length| length > state.max_download_bytes)
    {
        return Err(TaskError::TooLarge);
    }
    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(path).await?;
    let mut downloaded = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if downloaded > state.max_download_bytes {
            return Err(TaskError::TooLarge);
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

async fn serve_download(
    State(state): State<AppState>,
    AxumPath(file): AxumPath<String>,
) -> Result<Response, ApiError> {
    if !safe_file_name(&file) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_file",
            "文件名无效。",
        ));
    }
    let path = state.download_dir.join(&file);
    let source = tokio::fs::File::open(&path)
        .await
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "file_not_found", "文件不存在。"))?;
    let size = source
        .metadata()
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let stream = futures_util::stream::try_unfold(source, |mut source| async move {
        let mut chunk = vec![0_u8; 64 * 1024];
        let read = source.read(&mut chunk).await?;
        if read == 0 {
            Ok::<_, std::io::Error>(None)
        } else {
            chunk.truncate(read);
            Ok(Some((chunk, source)))
        }
    });
    let content_type = download_content_type(&file);
    Response::builder()
        .header(CONTENT_TYPE, content_type)
        .header(CONTENT_LENGTH, size)
        .header(
            CONTENT_DISPOSITION,
            format!("attachment; filename=\"{file}\""),
        )
        .body(Body::from_stream(stream))
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response_error",
                "无法创建下载响应。",
            )
        })
}

async fn serve_index(State(state): State<AppState>) -> Result<Response, ApiError> {
    static_file_response(&state.web_dir.join("index.html"), false).await
}

async fn serve_asset(
    State(state): State<AppState>,
    AxumPath(path): AxumPath<String>,
) -> Result<Response, ApiError> {
    let safe = path.split('/').all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
    });
    if safe {
        let file = state.web_dir.join(&path);
        if file.is_file() {
            return static_file_response(&file, true).await;
        }
    }
    static_file_response(&state.web_dir.join("index.html"), false).await
}

async fn static_file_response(path: &Path, cache: bool) -> Result<Response, ApiError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "not_found", "页面资源不存在。"))?;
    let content_type = match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "json" => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    };
    Response::builder()
        .header(CONTENT_TYPE, content_type)
        .header(
            CACHE_CONTROL,
            if cache {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            },
        )
        .body(Body::from(bytes))
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response_error",
                "无法创建页面响应。",
            )
        })
}

fn safe_file_name(value: &str) -> bool {
    let extension = value
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    !value.is_empty()
        && value.len() <= 160
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        && !value.starts_with('.')
        && matches!(
            extension.as_str(),
            "mp4" | "jpg" | "jpeg" | "png" | "webp" | "gif" | "avif"
        )
}

fn download_content_type(file: &str) -> &'static str {
    match file
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" => "video/mp4",
        "png" => "image/png",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "gif" => "image/gif",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
}

async fn fetch_media_response(
    state: &AppState,
    media_url: &str,
) -> Result<reqwest::Response, TaskError> {
    let mut current = validate_download_url(media_url)?;
    for _ in 0..=5 {
        let response = state
            .client
            .get(current.clone())
            .header(USER_AGENT, resolver_core::MOBILE_USER_AGENT)
            .header(ACCEPT, "video/*,application/octet-stream;q=0.9,*/*;q=0.5")
            .header(REFERER, "https://www.douyin.com/")
            .send()
            .await?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| TaskError::Internal("媒体重定向缺少地址。".to_string()))?;
            let next = current
                .join(location)
                .map_err(|_| TaskError::Internal("媒体重定向地址无效。".to_string()))?;
            current = validate_download_url(next.as_str())?;
            continue;
        }
        if !response.status().is_success() {
            return Err(TaskError::DownloadHttp(response.status().as_u16()));
        }
        if response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                let value = value.to_ascii_lowercase();
                value.starts_with("text/") || value.starts_with("application/json")
            })
        {
            return Err(TaskError::InvalidContent);
        }
        return Ok(response);
    }
    Err(TaskError::Internal("媒体重定向次数过多。".to_string()))
}

async fn update_task(tasks: &TaskStore, id: Uuid, update: impl FnOnce(&mut TaskRecord)) {
    if let Some(task) = tasks.write().await.get_mut(&id) {
        update(task);
    }
}

fn public_error_message(error: &TaskError) -> String {
    match error {
        TaskError::Resolve(error) => error.error.clone(),
        TaskError::NoMedia => "未找到可下载的视频地址。".to_string(),
        TaskError::TooLarge => "文件超过服务器允许的大小。".to_string(),
        TaskError::Http(error) => {
            if error.is_timeout() {
                "服务器下载媒体超时，请稍后重试。".to_string()
            } else if error.is_connect() {
                "服务器无法连接媒体 CDN，请检查网络或代理设置。".to_string()
            } else {
                format!("服务器下载媒体失败：{error}")
            }
        }
        TaskError::DownloadHttp(status) => format!("媒体 CDN 返回 HTTP {status}。"),
        TaskError::InvalidContent => "媒体 CDN 返回了网页或 JSON，而不是视频文件。".to_string(),
        TaskError::Io(error) => format!("服务器保存文件失败：{error}"),
        TaskError::Internal(error) => error.clone(),
    }
}

#[derive(Debug)]
enum TaskError {
    Resolve(ResolveError),
    Http(reqwest::Error),
    DownloadHttp(u16),
    InvalidContent,
    Io(std::io::Error),
    NoMedia,
    TooLarge,
    Internal(String),
}

impl fmt::Display for TaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(error) => write!(formatter, "{error}"),
            Self::Http(error) => write!(formatter, "{error}"),
            Self::DownloadHttp(status) => write!(formatter, "media CDN returned HTTP {status}"),
            Self::InvalidContent => formatter.write_str("media CDN returned non-media content"),
            Self::Io(error) => write!(formatter, "{error}"),
            Self::NoMedia => formatter.write_str("no media URL in resolver response"),
            Self::TooLarge => formatter.write_str("download exceeds configured size limit"),
            Self::Internal(error) => formatter.write_str(error),
        }
    }
}

impl From<ResolveError> for TaskError {
    fn from(value: ResolveError) -> Self {
        Self::Resolve(value)
    }
}
impl From<reqwest::Error> for TaskError {
    fn from(value: reqwest::Error) -> Self {
        Self::Http(value)
    }
}
impl From<std::io::Error> for TaskError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_download_base_is_restricted_to_a_safe_path() {
        assert_eq!(normalize_public_base("/downloads/").unwrap(), "/downloads");
        assert!(normalize_public_base("downloads").is_err());
        assert!(normalize_public_base("/../private").is_err());
    }

    #[test]
    fn task_file_name_only_uses_safe_video_id_characters() {
        let source = "12/../ab-CD";
        let safe = source
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>();
        assert_eq!(safe, "12abCD");
    }
}
