use sentry::protocol::{Event, Exception, Frame, Stacktrace};
use std::{sync::Arc, time::Duration};

fn private_event(event: Event<'static>) -> Option<Event<'static>> {
    let exception = event
        .exception
        .values
        .into_iter()
        .map(|exception| Exception {
            ty: exception.ty,
            value: Some("Unexpected uploader error".into()),
            stacktrace: exception.stacktrace.map(|trace| Stacktrace {
                frames: trace
                    .frames
                    .into_iter()
                    .map(|frame| Frame {
                        function: frame.function,
                        filename: frame
                            .filename
                            .and_then(|name| name.rsplit(['/', '\\']).next().map(String::from)),
                        lineno: frame.lineno,
                        colno: frame.colno,
                        in_app: frame.in_app,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect::<Vec<_>>()
        .into();
    Some(Event {
        event_id: event.event_id,
        timestamp: event.timestamp,
        level: event.level,
        platform: event.platform,
        release: event.release,
        environment: event.environment,
        sdk: event.sdk,
        exception,
        message: Some("Unexpected uploader error".into()),
        tags: [("component".into(), "uploader".into())].into(),
        ..Default::default()
    })
}

pub fn init() -> sentry::ClientInitGuard {
    let dsn = std::env::var("SENTRY_DSN")
        .ok()
        .or_else(|| {
            option_env!("AZS_SENTRY_DSN")
                .filter(|dsn| !dsn.is_empty())
                .map(String::from)
        })
        .or_else(|| {
            if cfg!(debug_assertions) {
                return None;
            }
            serde_json::from_str::<serde_json::Value>(include_str!("../../../infra/sentry.json"))
                .ok()
                .and_then(|config| {
                    config["projects"]["uploader"]["dsn"]
                        .as_str()
                        .map(String::from)
                })
        })
        .and_then(|value| value.parse().ok());
    let mut options = sentry::ClientOptions::default();
    options.dsn = dsn;
    options.release = Some(concat!("azs-uploader@", env!("CARGO_PKG_VERSION")).into());
    options.environment = Some(
        std::env::var("SENTRY_ENVIRONMENT")
            .unwrap_or_else(|_| "production".into())
            .into(),
    );
    options.send_default_pii = false;
    options.max_breadcrumbs = 0;
    options.shutdown_timeout = Duration::from_secs(1);
    options.before_send = Some(Arc::new(private_event));
    sentry::init(options)
}

pub fn capture(error: &anyhow::Error) {
    sentry::capture_error::<dyn std::error::Error>(error.as_ref());
}
