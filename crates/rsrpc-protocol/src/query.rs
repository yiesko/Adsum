//! Query-string parsing shared by the game and bridge transports.
//!
//! Exact legacy `get_url_params` semantics: no `?` yields nothing, pairs
//! without `=` are ignored, no percent-decoding (Discord clients send
//! plain ASCII here).

/// Read one value straight off the query of `uri` (`/path?a=1&b=2`).
///
/// The value is a slice of `uri`, so looking a parameter up costs no
/// allocation and no decoding — unlike building the whole parameter map
/// to read a single key out of it. A key repeated in the query answers
/// with the **last** occurrence, which is what inserting the pairs into
/// a map in order used to give.
#[must_use]
pub fn query_param<'a>(uri: &'a str, key: &str) -> Option<&'a str> {
  let (_, query) = uri.split_once('?')?;
  let mut found = None;
  for pair in query.split('&') {
    if let Some((k, value)) = pair.split_once('=')
      && k == key
    {
      found = Some(value);
    }
  }
  found
}
