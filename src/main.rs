use std::convert::Infallible;
use std::future::Future;
use std::task::{ready, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::{Buf, Bytes, BytesMut};
use http_body::{Body as HttpBody, Frame, SizeHint};
use tokio::net::TcpListener;
use tokio::time::Sleep;
use tokio_util::io::ReaderStream;
use tower::Service;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app = Router::new()
        .route("/:count", get(handler))
        .route("/yolo", get(swag))
        .layer(ThrottleLayer::new(10, Duration::from_secs(1)));

    let listener = TcpListener::bind("0.0.0.0:25565").await?;

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            _ = tokio::signal::ctrl_c().await;
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(10));
                eprintln!("requests didn't finish within 10 seconds, forcefully shutting down...");
                std::process::exit(1)
            });
            println!("ctrl-c detected, waiting for requests to finish...")
        })
        .await?;

    Ok(())
}

async fn swag() -> Response {
    let f = tokio::fs::File::open("Cargo.toml").await.unwrap();
    let rs = ReaderStream::new(f);
    Body::from_stream(rs).into_response()
}

async fn handler(Path(count): Path<usize>) -> Response {
    Bytes::from_iter((b'a'..=b'z').cycle().take(count)).into_response()
}

#[derive(Clone)]
struct ThrottleLayer {
    bytes_per_period: usize,
    period: std::time::Duration,
}

impl ThrottleLayer {
    fn new(bytes_per_period: usize, period: std::time::Duration) -> Self {
        Self {
            bytes_per_period,
            period,
        }
    }
}

impl<S> tower::Layer<S> for ThrottleLayer {
    type Service = ThrottleService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ThrottleService {
            inner,
            bytes_per_period: self.bytes_per_period,
            period: self.period,
        }
    }
}

#[derive(Clone)]
struct ThrottleService<S> {
    inner: S,
    bytes_per_period: usize,
    period: std::time::Duration,
}

impl<S, R> Service<R> for ThrottleService<S>
where
    S: Service<R, Response = Response, Error = Infallible>,
{
    type Response = Response;

    type Error = Infallible;

    type Future = ThrottleFut<S::Future>;

    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        ThrottleFut {
            fut: self.inner.call(req),
            bytes_per_period: self.bytes_per_period,
            period: self.period,
        }
    }
}

#[pin_project::pin_project]
struct ThrottleFut<F> {
    #[pin]
    fut: F,
    bytes_per_period: usize,
    period: std::time::Duration,
}

impl<F> Future for ThrottleFut<F>
where
    F: Future<Output = Result<Response, Infallible>>,
{
    type Output = F::Output;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        this.fut.poll(cx).map(|resp| {
            resp.map(|response| {
                let (parts, body) = response.into_parts();
                let body = Body::new(ThrottleBody::new(
                    body,
                    *this.bytes_per_period,
                    *this.period,
                ));
                Response::from_parts(parts, body)
            })
        })
    }
}

#[pin_project::pin_project]
struct ThrottleBody<B> {
    #[pin]
    body: B,
    #[pin]
    sleep: Option<Sleep>,
    buf: BytesMut,
    period: std::time::Duration,
    max_read: usize,
    trailers: Option<HeaderMap>,
}

impl<B> ThrottleBody<B> {
    fn new(body: B, max_read: usize, period: Duration) -> Self {
        ThrottleBody {
            body,
            sleep: None,
            buf: BytesMut::new(),
            period,
            max_read,
            trailers: None,
        }
    }
}

impl<B> HttpBody for ThrottleBody<B>
where
    B: HttpBody,
{
    type Data = Bytes;

    type Error = B::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();

        if let Some(mut sleep) = this.sleep.as_mut().as_pin_mut() {
            ready!(sleep.as_mut().poll(cx));
            this.sleep.set(None);
        }

        while this.buf.len() < *this.max_read {
            let maybe_frame_result = ready!(this.body.as_mut().poll_frame(cx));
            let Some(frame_result) = maybe_frame_result else {
                break;
            };
            let frame = match frame_result {
                Ok(frame) => frame,
                Err(err) => return Poll::Ready(Some(Err(err))),
            };
            match frame.into_data() {
                Ok(mut data) => {
                    while data.remaining() > 0 {
                        let slice = data.chunk();
                        this.buf.extend_from_slice(slice);
                        data.advance(slice.len());
                    }
                    continue;
                }
                Err(frame) => {
                    if let Ok(trailers) = frame.into_trailers() {
                        *this.trailers = Some(trailers);
                    }
                }
            }
        }

        if !this.buf.is_empty() {
            let readable = this
                .buf
                .split_to(std::cmp::min(*this.max_read, this.buf.len()))
                .freeze();
            this.sleep.set(Some(tokio::time::sleep(*this.period)));
            return Poll::Ready(Some(Ok(Frame::data(readable))));
        }

        if let Some(hm) = this.trailers.take() {
            return Poll::Ready(Some(Ok(Frame::trailers(hm))));
        }

        Poll::Ready(None)
    }

    fn is_end_stream(&self) -> bool {
        self.buf.is_empty() && self.trailers.is_none() && self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        let size_hint_inner = self.body.size_hint();
        let lower = size_hint_inner.lower();
        let upper = size_hint_inner.upper();

        let mut size_hint = SizeHint::new();
        size_hint.set_lower(lower + self.buf.len() as u64);
        if let Some(upper) = upper {
            size_hint.set_upper(upper + self.buf.len() as u64);
        }
        size_hint
    }
}
