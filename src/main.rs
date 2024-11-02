#![warn(
    clippy::nursery,
    clippy::correctness,
    clippy::style,
    clippy::perf,
    clippy::complexity,
    clippy::cognitive_complexity,
    clippy::double_parens,
    clippy::len_zero,
    clippy::question_mark,
    clippy::suspicious,
    clippy::todo
)]

mod api;
mod game;
mod manager;
pub mod models;
pub mod schema;
mod util;

use std::{io::stdout, sync::Arc};

use anyhow::{anyhow, Context};
use axum::{
    extract::{MatchedPath, Request},
    Router,
};
use clap::Parser;
use diesel::pg::Pg;
use diesel_async::{
    async_connection_wrapper::AsyncConnectionWrapper,
    pooled_connection::{deadpool::Pool, AsyncDieselConnectionManager},
};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use fred::prelude::*;
use serde::Deserialize;
use steam_openid::SteamOpenId;
use steam_rs::Steam;
use time::Duration;
use tower_http::trace::TraceLayer;
use tower_sessions::{Expiry, SessionManagerLayer};
use tracing::{debug, info};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{
    fmt::writer::MakeWriterExt, layer::SubscriberExt, util::SubscriberInitExt,
};
use util::session_store::RedisStore;
use utoipa_scalar::{Scalar, Servable};

use crate::game::{routes_as, routes_steam, routes_steam_doubleslash};
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

/// Wavebreaker-specific user agent
pub const WAVEBREAKER_USER_AGENT: &str = concat!(
    concat!(
        concat!(env!("CARGO_PKG_NAME"), "/"),
        env!("CARGO_PKG_VERSION")
    ),
    concat!(concat!(" (", env!("CARGO_PKG_REPOSITORY")), ")")
);

#[derive(Deserialize, Clone)]
struct Config {
    main: Main,
    radio: Radio,
    external: External,
}

#[derive(Deserialize, Clone)]
struct Main {
    address: String,
    database: String,
    redis: String,
    jwt_secret: String,
}

#[derive(Deserialize, Clone)]
struct Radio {
    cgr_location: String,
}

#[derive(Deserialize, Clone)]
struct External {
    steam_key: String,
    steam_realm: String,
    steam_return_path: String,
}

#[derive(Clone)]
pub struct AppState {
    steam_api: Arc<Steam>,
    steam_openid: Arc<SteamOpenId>,
    config: Arc<Config>,
    db: Pool<diesel_async::AsyncPgConnection>,
    redis: Arc<RedisPool>,
    jwt_keys: util::jwt::Keys,
}

fn run_migrations(
    connection: &mut impl MigrationHarness<Pg>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    // This will run the necessary migrations.
    //
    // See the documentation for `MigrationHarness` for
    // all available methods.
    connection.run_pending_migrations(MIGRATIONS)?;

    Ok(())
}

/// Reads the config, initializes database connections and the Steam API client
///
/// # Returns
/// An `AppState` struct with all the necessary members
///
/// # Errors
/// This function can fail if the config file is missing or invalid, the connection to Postgres or Redis fails, or the Steam API key is invalid
async fn init_state() -> anyhow::Result<AppState> {
    let wavebreaker_config: Config = Figment::new()
        .merge(Toml::file("Wavebreaker.toml"))
        .merge(Env::prefixed("WAVEBREAKER_"))
        .extract()
        .context("Config should be valid!")?;

    let diesel_manager = AsyncDieselConnectionManager::<diesel_async::AsyncPgConnection>::new(
        &wavebreaker_config.main.database,
    );
    let pool = Pool::builder(diesel_manager)
        .build()
        .context("Failed to build DB pool!")?;

    // clone the url because moving the value will screw things up
    let pg_url = wavebreaker_config.main.database.clone();
    tokio::task::spawn_blocking(move || {
        use diesel::prelude::Connection;
        use diesel_async::pg::AsyncPgConnection;
        let mut conn = AsyncConnectionWrapper::<AsyncPgConnection>::establish(&pg_url)
            .expect("Failed to establish DB connection for migrations!");

        run_migrations(&mut conn).expect("Failed to run migrations!");
    })
    .await?;

    let redis_cfg = RedisConfig::from_url(&wavebreaker_config.main.redis)?;
    let redis_builder = Builder::from_config(redis_cfg);

    let redis_pool = redis_builder
        .build_pool(3)
        .context("Failed to build Redis pool!")?;

    redis_pool
        .init()
        .await
        .context("Clients failed to connect to Redis!")?;

    musicbrainz_rs::config::set_user_agent(WAVEBREAKER_USER_AGENT);

    let steam_openid = SteamOpenId::new(
        &wavebreaker_config.external.steam_realm,
        &wavebreaker_config.external.steam_return_path,
    )
    .map_err(|e| anyhow!("Failed to construct SteamOpenId: {e:?}"))?;

    Ok(AppState {
        steam_api: Arc::new(Steam::new(&wavebreaker_config.external.steam_key)),
        steam_openid: Arc::new(steam_openid),
        db: pool,
        redis: Arc::new(redis_pool),
        jwt_keys: util::jwt::Keys::new(wavebreaker_config.main.jwt_secret.as_bytes()),
        config: Arc::new(wavebreaker_config),
    })
}

fn make_router(state: AppState) -> Router {
    let (api_router, openapi) = api::routes();

    let session_store = RedisStore::new((*state.redis).clone());
    //TODO: Make with_secure configurable
    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(true)
        .with_expiry(Expiry::OnInactivity(Duration::days(30)));

    Router::new()
        .nest("/as_steamlogin", routes_steam())
        .nest("//as_steamlogin", routes_steam_doubleslash()) // for that one edge case
        .nest("/as", routes_as(&state.config.radio.cgr_location))
        .nest("/api", api_router)
        .merge(Scalar::with_url("/api/docs", openapi))
        .layer(session_layer)
        .layer(
            // TAKEN FROM: https://github.com/tokio-rs/axum/blob/d1fb14ead1063efe31ae3202e947ffd569875c0b/examples/error-handling/src/main.rs#L60-L77
            TraceLayer::new_for_http() // Create our own span for the request and include the matched path. The matched
                // path is useful for figuring out which handler the request was routed to.
                .make_span_with(|req: &Request| {
                    let method = req.method();
                    let uri = req.uri();

                    // axum automatically adds this extension.
                    let matched_path = req
                        .extensions()
                        .get::<MatchedPath>()
                        .map(axum::extract::MatchedPath::as_str);

                    tracing::debug_span!("request", %method, %uri, matched_path)
                })
                // By default `TraceLayer` will log 5xx responses but we're doing our specific
                // logging of errors so disable that
                .on_failure(()),
        )
        .with_state(state)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let file_appender = RollingFileAppender::builder()
        .filename_suffix("wavebreaker.log")
        .rotation(Rotation::DAILY)
        .build("./logs")
        .expect("Initializing logging failed");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // axum logs rejections from built-in extractors with the `axum::rejection`
                // target, at `TRACE` level. `axum::rejection=trace` enables showing those events
                "wavebreaker=info,tower_http=error,axum::rejection=trace".into()
            }),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(stdout.and(non_blocking)))
        .init();

    debug!("Start init");

    let state = init_state().await?;

    // Parse CLI arguments
    // and if we have a management command, don't spin up a server
    let args = manager::Args::parse();
    if args.command.is_some() {
        return manager::parse_command(&args.command.unwrap(), state).await;
    }

    info!("Wavebreaker starting...");

    let listener = tokio::net::TcpListener::bind(&state.config.main.address)
        .await
        .context("Listener should always be able to listen!")?;
    info!("Listening on {}", &state.config.main.address);

    let app = make_router(state);

    axum::serve(listener, app)
        .await
        .context("Server should be able to... well, serve!")
}
