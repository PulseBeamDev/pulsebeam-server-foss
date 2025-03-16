use axum::routing::get;
use std::net::SocketAddr;
use tracing::{info, level_filters::LevelFilter};
use tracing_subscriber::fmt::format::FmtSpan;

use pulsebeam_server_foss::proto::signaling_server::SignalingServer;
use pulsebeam_server_foss::server::Server;
use std::time::Duration;
use tonic::service::LayerExt;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let env_filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .compact()
        .with_span_events(FmtSpan::NONE)
        .with_env_filter(env_filter)
        .init();

    let cors = CorsLayer::very_permissive()
        // https://github.com/tower-rs/tower-http/issues/194
        .allow_origin(AllowOrigin::mirror_request())
        .max_age(Duration::from_secs(86400));
    let server = SignalingServer::new(Server::default());
    let server = tower::ServiceBuilder::new()
        .layer(tonic_web::GrpcWebLayer::new())
        .into_inner()
        .named_layer(server);
    let grpc_routes = tonic::service::Routes::new(server)
        .prepare()
        .into_axum_router()
        .layer(cors)
        .layer(tower_http::trace::TraceLayer::new_for_grpc());

    let addr: SocketAddr = "[::]:3000".parse().unwrap();
    info!("Listening on {addr}");

    let router = axum::Router::new()
        .nest_service("/grpc", grpc_routes)
        .route("/_ping", get(ping));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;

    Ok(())
}

async fn ping() -> &'static str {
    "Pong\n"
}
