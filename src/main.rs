use std::{error::Error, sync::Arc};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    serve, Router,
};
use chrono::NaiveDate;
use gray_matter::{engine::TOML, Matter};
use lastfm::Client;
use markdown::{
    message::Message, to_html_with_options, CompileOptions, Constructs, Options, ParseOptions,
};
use serde::{Deserialize, Serialize};
use tera::{Context, Tera};
use tokio::{
    fs::{read_dir, File},
    io::AsyncReadExt,
    net::TcpListener,
};
use tower_http::services::ServeDir;

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct SharedState {
    tera: Tera,
    lastfm: Client<String, String>,
}

enum AppError {
    Template(tera::Error),
    Io(std::io::Error),
    Markdown(Message),
    Frontmatter(String),
    LastFm(lastfm::errors::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            match self {
                Self::Template(e) => e.to_string(),
                Self::Io(e) => e.to_string(),
                Self::Markdown(e) => e.to_string(),
                Self::Frontmatter(e) => format!("failed to parse frontmatter for {}", e),
                Self::LastFm(e) => e.to_string(),
            },
        )
            .into_response()
    }
}

impl From<tera::Error> for AppError {
    fn from(value: tera::Error) -> Self {
        Self::Template(value)
    }
}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<Message> for AppError {
    fn from(value: Message) -> Self {
        Self::Markdown(value)
    }
}

impl From<lastfm::errors::Error> for AppError {
    fn from(value: lastfm::errors::Error) -> Self {
        Self::LastFm(value)
    }
}

#[derive(Serialize, Deserialize)]
struct BlogInfo {
    title: String,
    date: NaiveDate,
    slug: String,
}

#[derive(Serialize, Deserialize)]
struct PageInfo {
    title: String,
}

fn parse_markdown(content: &str) -> Result<String, Message> {
    // annoying that we have to allocate the Options every time
    // but currently Options is not Send/Sync: https://github.com/wooorm/markdown-rs/issues/104
    to_html_with_options(
        content,
        &Options {
            compile: CompileOptions {
                allow_dangerous_html: true,
                ..Default::default()
            },
            parse: ParseOptions {
                constructs: Constructs {
                    frontmatter: true,
                    gfm_table: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        },
    )
}

fn render_template(tera: &Tera, name: &str, context: &mut Context) -> Result<String, tera::Error> {
    context.insert("version", VERSION);
    tera.render(name, context)
}

async fn index(State(state): State<Arc<SharedState>>) -> Result<Html<String>, AppError> {
    let mut context = Context::new();
    let mut content = String::new();
    File::open("content/index.md")
        .await?
        .read_to_string(&mut content)
        .await?;
    let frontmatter = Matter::<TOML>::new()
        .parse_with_struct::<PageInfo>(&content)
        .ok_or(AppError::Frontmatter("content/index.md".to_string()))?
        .data;
    context.insert("title", &frontmatter.title);
    context.insert("active", &frontmatter.title.to_lowercase());
    context.insert("content", &parse_markdown(&content)?);
    Ok(Html(render_template(
        &state.tera,
        "basic.html",
        &mut context,
    )?))
}

async fn blog(State(state): State<Arc<SharedState>>) -> Result<Html<String>, AppError> {
    let mut context = Context::new();
    let mut folder = read_dir("content/blog").await?;
    let mut posts = Vec::new();
    while let Some(entry) = folder.next_entry().await? {
        let mut content = String::new();
        if entry.file_type().await?.is_file() {
            let filename_lossy = entry.file_name().to_string_lossy().to_string();
            File::open(entry.path())
                .await?
                .read_to_string(&mut content)
                .await?;
            let frontmatter = Matter::<TOML>::new()
                .parse_with_struct::<BlogInfo>(&content)
                .ok_or(AppError::Frontmatter(filename_lossy))?
                .data;
            posts.push(frontmatter);
        }
    }
    posts.reverse();
    context.insert("active", "blog");
    context.insert("posts", &posts);
    Ok(Html(render_template(
        &state.tera,
        "blog.html",
        &mut context,
    )?))
}

async fn blog_post(
    State(state): State<Arc<SharedState>>,
    Path(slug): Path<String>,
) -> Result<Html<String>, AppError> {
    let file_path = format!("content/blog/{}.md", slug);
    let mut context = Context::new();
    let mut content = String::new();
    File::open(&file_path)
        .await?
        .read_to_string(&mut content)
        .await?;
    let frontmatter = Matter::<TOML>::new()
        .parse_with_struct::<BlogInfo>(&content)
        .ok_or(AppError::Frontmatter(file_path))?
        .data;
    context.insert("post", &frontmatter);
    context.insert("content", &parse_markdown(&content)?);
    Ok(Html(render_template(
        &state.tera,
        "blog-post.html",
        &mut context,
    )?))
}

async fn projects(State(state): State<Arc<SharedState>>) -> Result<Html<String>, AppError> {
    let mut context = Context::new();
    context.insert("active", "projects");
    Ok(Html(render_template(
        &state.tera,
        "projects.html",
        &mut context,
    )?))
}

async fn changelog(State(state): State<Arc<SharedState>>) -> Result<Html<String>, AppError> {
    let mut context = Context::new();
    let mut content = String::new();
    File::open("content/changelog.md")
        .await?
        .read_to_string(&mut content)
        .await?;
    let frontmatter = Matter::<TOML>::new()
        .parse_with_struct::<PageInfo>(&content)
        .ok_or(AppError::Frontmatter("content/changelog.md".to_string()))?
        .data;
    context.insert("title", &frontmatter.title);
    context.insert("content", &parse_markdown(&content)?);
    Ok(Html(render_template(
        &state.tera,
        "basic.html",
        &mut context,
    )?))
}

async fn currently_playing(
    State(state): State<Arc<SharedState>>,
) -> Result<Html<String>, AppError> {
    let mut context = Context::new();
    let track = state.lastfm.now_playing().await?;
    context.insert("track", &track);
    match render_template(&state.tera, "currently-playing.html", &mut context) {
        Ok(content) => Ok(Html(content)),
        Err(e) => Ok(Html(format!("<span id=\"track\" class=\"has-text-danger\">{}</span>", e))),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let tera = Tera::new("templates/**/*.html")?;
    let lastfm = Client::<String, String>::try_from_env("bobertoyin".to_string())?;
    let app = Router::new()
        .route("/", get(index))
        .route("/blog", get(blog))
        .route("/blog/:slug", get(blog_post))
        .route("/projects", get(projects))
        .route("/changelog", get(changelog))
        .route("/currently_playing", get(currently_playing))
        .with_state(Arc::new(SharedState { tera, lastfm }))
        .nest_service("/static", ServeDir::new("static"));
    let listener = TcpListener::bind("0.0.0.0:3000").await?;
    Ok(serve(listener, app).await?)
}
