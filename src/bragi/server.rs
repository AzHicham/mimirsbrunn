use snafu::{ResultExt, Snafu};
use std::net::ToSocketAddrs;
use tracing::{info, instrument};
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_log::LogTracer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Registry};
use warp::Filter;

use super::settings::{Error as SettingsError, Opts, Settings};
use mimir2::{
    adapters::primary::bragi::api::{forward_geocoder, reverse_geocoder, status},
    adapters::primary::bragi::{handlers, routes},
    adapters::secondary::elasticsearch::remote::{
        connection_pool_url, Error as ElasticsearchRemoteError,
    },
    domain::ports::secondary::remote::{Error as PortRemoteError, Remote},
};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not create an Elasticsearch Connection Pool: {}", source))]
    ElasticsearchConnectionPoolCreation { source: ElasticsearchRemoteError },

    #[snafu(display("Could not establish Elasticsearch Connection: {}", source))]
    ElasticsearchConnection { source: PortRemoteError },

    #[snafu(display("Could not generate settings: {}", source))]
    SettingsProcessing { source: SettingsError },

    #[snafu(display("Socket Addr Error {}", source))]
    SockAddr { source: std::io::Error },

    #[snafu(display("Addr Resolution Error {}", msg))]
    AddrResolution { msg: String },
}

#[allow(clippy::needless_lifetimes)]
pub async fn run(opts: &Opts) -> Result<(), Error> {
    let settings = Settings::new(opts).context(SettingsProcessing)?;
    LogTracer::init().expect("Unable to setup log tracer!");

    // following code mostly from https://betterprogramming.pub/production-grade-logging-in-rust-applications-2c7fffd108a6
    let app_name = concat!(env!("CARGO_PKG_NAME"), "-", env!("CARGO_PKG_VERSION")).to_string();

    let file_appender = tracing_appender::rolling::daily(&settings.logging.path, "mimir.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let bunyan_formatting_layer = BunyanFormattingLayer::new(app_name, non_blocking);
    let subscriber = Registry::default()
        .with(EnvFilter::new("INFO"))
        .with(JsonStorageLayer)
        .with(bunyan_formatting_layer);
    tracing::subscriber::set_global_default(subscriber).expect("tracing subscriber global default");

    run_server(settings).await
}

#[instrument(skip(settings))]
pub async fn run_server(settings: Settings) -> Result<(), Error> {
    let host = settings.elasticsearch.host;
    let port = settings.elasticsearch.port;
    let addr = (host.as_str(), port);
    let addr = addr
        .to_socket_addrs()
        .context(SockAddr)?
        .next()
        .ok_or(Error::AddrResolution {
            msg: String::from("Cannot resolve elasticsearch addr"),
        })?;
    let elasticsearch_url = addr.to_string();

    let pool = connection_pool_url(&elasticsearch_url)
        .await
        .context(ElasticsearchConnectionPoolCreation)?;

    let client = pool
        .conn(
            settings.elasticsearch.timeout,
            &settings.elasticsearch.version_req,
        )
        .await
        .context(ElasticsearchConnection)?;

    let api = reverse_geocoder!(client.clone(), settings.query.clone())
        .or(forward_geocoder!(client.clone(), settings.query))
        .or(status!(client, elasticsearch_url))
        .recover(routes::report_invalid)
        .with(warp::trace::request());

    let host = settings.service.host;
    let port = settings.service.port;
    let addr = (host.as_str(), port);
    let addr = addr
        .to_socket_addrs()
        .context(SockAddr)?
        .next()
        .ok_or(Error::AddrResolution {
            msg: String::from("Cannot resolve addr"),
        })?;

    info!("Serving bragi on {}", addr);

    warp::serve(api).run(addr).await;

    Ok(())
}
