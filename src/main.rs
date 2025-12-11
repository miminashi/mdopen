use minijinja::{context, Environment};
use percent_encoding::percent_decode;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Cursor};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

mod app_config;
mod cli;
mod markdown;

#[cfg(feature = "reload")]
mod watch;

#[cfg(feature = "reload")]
mod websocket;

#[cfg(feature = "syntax")]
mod syntax;

use app_config::AppConfig;

pub static STYLE_CSS: &[u8] = include_bytes!("vendor/github.css");

pub static ASSETS_PREFIX: &str = "/__mdopen_assets/";
pub static RELOAD_PREFIX: &str = "/__mdopen_reload/";

fn html_response(text: impl Into<Vec<u8>>, status: StatusCode) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(text.into())
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf8"[..]).unwrap(),
        )
        .with_status_code(status)
}

fn error_response(error_code: StatusCode, jinja_env: &Environment) -> Response<Cursor<Vec<u8>>> {
    let tpl = jinja_env.get_template("error.html").unwrap();
    let html = tpl
        .render(context! {
            title => "Error",
            error_header => error_code.default_reason_phrase(),
        })
        .unwrap();
    html_response(html, StatusCode(404))
}

/// Get content type from extension.
fn mime_type(ext: &str) -> Option<&'static str> {
    match ext {
        "js" => Some("application/javascript"),
        "css" => Some("text/css"),
        "gif" => Some("image/gif"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "pdf" => Some("application/pdf"),
        "html" => Some("text/html"),
        "md" => Some("text/markdown"),
        "txt" => Some("text/plain"),
        _ => None,
    }
}

/// Returns response for static content request
fn handle_asset(path: &str, jinja_env: &Environment) -> Response<Cursor<Vec<u8>>> {
    let data = match path {
        "style.css" => STYLE_CSS,
        _ => {
            log::info!("asset not found: {}", &path);
            return error_response(StatusCode(404), jinja_env);
        }
    };

    Response::from_data(data)
        .with_header(Header::from_bytes(&b"Cache-Control"[..], &b"max-age=31536000"[..]).unwrap())
        .with_status_code(200)
}

/// Sort order for directory listing
#[derive(Clone, Copy, PartialEq)]
enum SortOrder {
    NameAsc,
    NameDesc,
    TimeAsc,
    TimeDesc,
}

impl SortOrder {
    fn from_query(query: Option<&str>) -> Self {
        match query {
            Some("name_desc") => SortOrder::NameDesc,
            Some("time") => SortOrder::TimeDesc,
            Some("time_asc") => SortOrder::TimeAsc,
            _ => SortOrder::NameAsc,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            SortOrder::NameAsc => "name",
            SortOrder::NameDesc => "name_desc",
            SortOrder::TimeAsc => "time_asc",
            SortOrder::TimeDesc => "time",
        }
    }
}

/// Format SystemTime as human-readable string
fn format_time(time: SystemTime) -> String {
    let duration = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();

    // Convert to datetime components (simplified UTC)
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;

    // Calculate year, month, day from days since epoch
    let mut year = 1970i32;
    let mut remaining_days = days as i32;

    loop {
        let days_in_year = if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
            366
        } else {
            365
        };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    let is_leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_months = if is_leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1;
    for days_in_month in days_in_months.iter() {
        if remaining_days < *days_in_month {
            break;
        }
        remaining_days -= *days_in_month;
        month += 1;
    }
    let day = remaining_days + 1;

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        year, month, day, hours, minutes
    )
}

// Get file contents for server response
// For directory, create listing in HTML
// For markdown, create generate HTML
// For other files, get its content
fn get_contents(
    path: &Path,
    config: &AppConfig,
    jinja_env: &Environment,
    sort_order: SortOrder,
) -> io::Result<Vec<u8>> {
    let cwd = env::current_dir()?;

    let absolute_path = cwd.join(path);

    let file_path = absolute_path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("mdopen");

    let Ok(metadata) = absolute_path.metadata() else {
        return Err(io::Error::new(io::ErrorKind::NotFound, "not found"));
    };

    if metadata.is_dir() {
        let entries = fs::read_dir(&absolute_path)?;

        #[derive(serde::Serialize)]
        struct DirItem {
            pub name: String,
            pub path: String,
            pub modified: String,
            pub modified_ts: u64,
            pub is_dir: bool,
        }

        // Build breadcrumb navigation
        #[derive(serde::Serialize)]
        struct BreadcrumbItem {
            pub name: String,
            pub path: String,
        }
        let mut breadcrumbs: Vec<BreadcrumbItem> = Vec::new();
        let mut cumulative_path = PathBuf::new();
        for component in path.components() {
            let name = component.as_os_str().to_string_lossy().to_string();
            cumulative_path.push(&name);
            breadcrumbs.push(BreadcrumbItem {
                name,
                path: cumulative_path.to_string_lossy().to_string(),
            });
        }

        let mut files: Vec<DirItem> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let file_name = e
                    .path()
                    .file_name()
                    .expect("filename")
                    .to_string_lossy()
                    .into_owned();
                let file_path = path.join(&file_name).to_string_lossy().to_string();
                let metadata = e.metadata().ok()?;
                let modified = metadata.modified().ok()?;
                let modified_ts = modified
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                Some(DirItem {
                    name: file_name,
                    path: file_path,
                    modified: format_time(modified),
                    modified_ts,
                    is_dir: metadata.is_dir(),
                })
            })
            .collect();

        // Sort files
        match sort_order {
            SortOrder::NameAsc => files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            SortOrder::NameDesc => files.sort_by(|a, b| b.name.to_lowercase().cmp(&a.name.to_lowercase())),
            SortOrder::TimeAsc => files.sort_by(|a, b| a.modified_ts.cmp(&b.modified_ts)),
            SortOrder::TimeDesc => files.sort_by(|a, b| b.modified_ts.cmp(&a.modified_ts)),
        }

        // Determine next sort for each column header
        let name_next_sort = match sort_order {
            SortOrder::NameAsc => "name_desc",
            _ => "name",
        };
        let time_next_sort = match sort_order {
            SortOrder::TimeDesc => "time_asc",
            _ => "time",
        };

        let tpl = jinja_env.get_template("dir.html").unwrap();
        let html = tpl
            .render(context! {
                dir_path => path,
                breadcrumbs => breadcrumbs,
                files => files,
                current_sort => sort_order.as_str(),
                name_next_sort => name_next_sort,
                time_next_sort => time_next_sort,
            })
            .unwrap();

        return Ok(html.into_bytes());
    }

    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    let data = fs::read(&absolute_path)?;

    let data = match ext {
        "md" | "markdown" => {
            let data = String::from_utf8_lossy(&data).to_string();
            let body = markdown::to_html(&data, config);

            let tpl = jinja_env.get_template("page.html").unwrap();
            let html = tpl
                .render(context! {
                    websocket_url => format!("ws://{}{}", config.addr, RELOAD_PREFIX), // FIXME: add file path
                    style_url => format!("{}style.css", ASSETS_PREFIX),
                    title => file_path,
                    markdown_body => body,
                    raw_url => format!("/{}/raw", path.display()),
                    enable_latex => config.enable_latex,
                    enable_reload => cfg!(feature = "reload") && config.enable_reload,
                })
                .unwrap();
            html.into()
        }
        _ => data,
    };
    Ok(data)
}
/// Serve raw markdown file without rendering
fn serve_raw_file(url: &str, jinja_env: &Environment) -> Response<Cursor<Vec<u8>>> {
    let path = PathBuf::from(
        percent_decode(url.as_bytes())
            .decode_utf8_lossy()
            .into_owned(),
    );
    let path_rel = path.strip_prefix("/").expect("url should have / prefix");

    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(_) => return error_response(StatusCode(500), jinja_env),
    };
    let absolute_path = cwd.join(path_rel);

    match fs::read(&absolute_path) {
        Ok(data) => Response::from_data(data)
            .with_status_code(200)
            .with_header(
                Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..]).unwrap(),
            ),
        Err(err) => {
            if err.kind() == io::ErrorKind::NotFound {
                error_response(StatusCode(404), jinja_env)
            } else {
                error_response(StatusCode(500), jinja_env)
            }
        }
    }
}

fn serve_file(url: &str, config: &AppConfig, jinja_env: &Environment) -> Response<Cursor<Vec<u8>>> {
    // Split URL into path and query string
    let (url_path, query_string) = match url.find('?') {
        Some(pos) => (&url[..pos], Some(&url[pos + 1..])),
        None => (url, None),
    };

    // Parse sort parameter from query string
    let sort_param = query_string.and_then(|qs| {
        qs.split('&')
            .find_map(|param| {
                let mut parts = param.splitn(2, '=');
                match (parts.next(), parts.next()) {
                    (Some("sort"), Some(value)) => Some(value),
                    _ => None,
                }
            })
    });
    let sort_order = SortOrder::from_query(sort_param);

    let path = PathBuf::from(
        percent_decode(url_path.as_bytes())
            .decode_utf8_lossy()
            .into_owned(),
    );
    let path_rel = path.strip_prefix("/").expect("url should have / prefix");
    let contents = get_contents(path_rel, config, jinja_env, sort_order);
    match contents {
        Ok(contents) => {
            let mut response = Response::from_data(contents).with_status_code(200);

            let ext = path_rel
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or_default();

            // FIXME: should this be in get_contents()?
            let mime = match mime_type(ext) {
                Some("text/markdown") => Some("text/html"),
                m => m,
            };
            if let Some(mime) = mime {
                response =
                    response.with_header(Header::from_bytes(&b"Content-Type"[..], mime).unwrap());
            }

            response
        }
        Err(err) => {
            if err.kind() == io::ErrorKind::NotFound {
                error_response(StatusCode(404), jinja_env)
            } else {
                error_response(StatusCode(500), jinja_env)
            }
        }
    }
}

/// Route a request and respond to it.
fn handle(
    request: Request,
    config: &AppConfig,
    jinja_env: &Environment,
    #[cfg(feature = "reload")] watcher_bus: Option<watch::WatcherBus>,
) {
    if request.method() != &Method::Get {
        let response = error_response(StatusCode(405), jinja_env);
        let _ = request.respond(response);
        return;
    }
    let url = request.url().to_owned();

    #[cfg(feature = "reload")]
    if let Some(path) = url.strip_prefix(RELOAD_PREFIX) {
        if let Some(watcher_bus) = watcher_bus {
            websocket::accept_websocket(request, watcher_bus);
        } else {
            log::warn!(
                "file watcher is disabled but websocket tried to connect to {}",
                path
            );
        }
        return;
    }

    let response = if let Some(path) = url.strip_prefix(ASSETS_PREFIX) {
        handle_asset(path, jinja_env)
    } else if let Some(path) = url.strip_suffix("/raw") {
        log::info!("raw endpoint requested: {}", path);
        serve_raw_file(path, jinja_env)
    } else {
        serve_file(&url, config, jinja_env)
    };
    if let Err(err) = request.respond(response) {
        log::error!("cannot respond: {}", err);
    };
}

#[cfg(feature = "open")]
fn open_browser(browser: &Option<String>, url: &str) -> io::Result<()> {
    match browser {
        Some(ref browser) => open::with(url, browser),
        None => open::that(url),
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = cli::CommandArgs::parse();
    let config = app_config::AppConfig {
        addr: SocketAddr::new(args.host, args.port),
        enable_reload: args.enable_reload,
        enable_latex: args.enable_latex,
        enable_syntax_highlight: args.enable_syntax_highlight,
    };

    let server = match Server::http(config.addr) {
        Ok(s) => s,
        Err(e) => {
            log::error!("cannot start server: {}", e);
            return;
        }
    };

    log::info!("serving at http://{}", config.addr);

    #[cfg(feature = "reload")]
    let (watcher_bus, _watcher) = if config.enable_reload {
        let (b, w) = watch::setup_watcher(&config);
        (Some(b), Some(w))
    } else {
        (None, None)
    };

    #[cfg(feature = "open")]
    if !args.files.is_empty() {
        std::thread::spawn(move || {
            for file in args.files.into_iter() {
                let url = format!("http://{}/{}", &config.addr, &file);
                log::info!("opening {}", &url);
                if let Err(e) = open_browser(&args.browser, &url) {
                    log::error!("cannot open browser: {}", e);
                }
            }
        });
    }

    let mut jinja_env = Environment::new();
    jinja_env.set_auto_escape_callback(|_filename| minijinja::AutoEscape::None);
    jinja_env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    jinja_env
        .add_template("base.html", include_str!("template/base.html"))
        .unwrap();
    jinja_env
        .add_template("page.html", include_str!("template/page.html"))
        .unwrap();
    jinja_env
        .add_template("dir.html", include_str!("template/dir.html"))
        .unwrap();
    jinja_env
        .add_template("error.html", include_str!("template/error.html"))
        .unwrap();

    for request in server.incoming_requests() {
        log::debug!("{} {}", request.method(), request.url());

        handle(
            request,
            &config,
            &jinja_env,
            #[cfg(feature = "reload")]
            watcher_bus.as_ref().map(|w| w.clone()),
        );
    }
}
