use crate::Error;
use serde::de::DeserializeSeed;

pub(super) async fn parse_full_response<T>(resp: reqwest::Response) -> Result<T, Error>
where
    T: serde::de::DeserializeOwned,
{
    let body_t0 = std::time::Instant::now();
    let bytes = resp.bytes().await?;
    let body_size = bytes.len();
    aube_util::diag::event_lazy(
        aube_util::diag::Category::Registry,
        "http_body_read",
        body_t0.elapsed(),
        || format!(r#"{{"bytes":{}}}"#, body_size),
    );
    // sonic-rs takes an immutable `&[u8]`, so we don't need to convert
    // `Bytes` into `BytesMut` (which previously cost a 5-50 MB to_vec
    // when the buffer wasn't exclusively owned). `Bytes::deref` is
    // already `&[u8]`, zero-copy regardless of refcount state.
    // sonic-rs is a strict superset of RFC 8259 for valid JSON; the
    // earlier serde_json fallback was dead weight on the happy path
    // and only collapsed into the same `Error::Io(InvalidData)` for
    // the user, so we keep the single-parser shape.
    let parse_t0 = std::time::Instant::now();
    let result = sonic_rs::from_slice::<T>(&bytes)
        .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)));
    aube_util::diag::event_lazy(
        aube_util::diag::Category::Registry,
        "json_parse_sonic_rs",
        parse_t0.elapsed(),
        || format!(r#"{{"bytes":{}}}"#, body_size),
    );
    result
}

pub(super) async fn parse_full_response_seed<S, T>(
    resp: reqwest::Response,
    seed: S,
) -> Result<T, Error>
where
    for<'de> S: DeserializeSeed<'de, Value = T>,
{
    let body_t0 = std::time::Instant::now();
    let bytes = resp.bytes().await?;
    let body_size = bytes.len();
    aube_util::diag::event_lazy(
        aube_util::diag::Category::Registry,
        "http_body_read",
        body_t0.elapsed(),
        || format!(r#"{{"bytes":{}}}"#, body_size),
    );
    let parse_t0 = std::time::Instant::now();
    let mut deserializer = sonic_rs::Deserializer::from_slice(&bytes);
    let result = seed
        .deserialize(&mut deserializer)
        .and_then(|value| {
            deserializer.end()?;
            Ok(value)
        })
        .map_err(|error| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error)));
    aube_util::diag::event_lazy(
        aube_util::diag::Category::Registry,
        "json_parse_sonic_rs",
        parse_t0.elapsed(),
        || format!(r#"{{"bytes":{}}}"#, body_size),
    );
    result
}
