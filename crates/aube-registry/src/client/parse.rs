use crate::Error;
use serde::de::DeserializeSeed;

pub(super) async fn parse_full_response<T>(resp: reqwest::Response) -> Result<T, Error>
where
    T: serde::de::DeserializeOwned,
{
    parse_full_response_with(resp, |bytes| sonic_rs::from_slice(&bytes)).await
}

pub(super) async fn parse_full_response_with<T>(
    resp: reqwest::Response,
    decode: impl FnOnce(bytes::Bytes) -> Result<T, sonic_rs::Error>,
) -> Result<T, Error> {
    let body_t0 = std::time::Instant::now();
    let bytes = resp.bytes().await?;
    let body_size = bytes.len();
    aube_util::diag::event_lazy(
        aube_util::diag::Category::Registry,
        "http_body_read",
        body_t0.elapsed(),
        || format!(r#"{{"bytes":{}}}"#, body_size),
    );
    // Passing ownership lets a selective decoder retain response slices without
    // copying the body. Ordinary typed decoders borrow the same buffer.
    let parse_t0 = std::time::Instant::now();
    let result = decode(bytes)
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
    parse_full_response_with(resp, |bytes| {
        let mut deserializer = sonic_rs::Deserializer::from_slice(&bytes);
        let value = seed.deserialize(&mut deserializer)?;
        deserializer.end()?;
        Ok(value)
    })
    .await
}
