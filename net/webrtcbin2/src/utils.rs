// SPDX-License-Identifier: MPL-2.0

pub fn parse_request_name<T, F: FnOnce(&str) -> Option<T>>(
    request: Option<&str>,
    prefix: &str,
    default: T,
    f: F,
) -> Option<T> {
    if let Some(name) = request {
        name.strip_prefix(prefix).and_then(|suffix| {
            if suffix.starts_with("%u") {
                Some(default)
            } else {
                f(suffix)
            }
        })
    } else {
        Some(default)
    }
}

pub fn parse_request_name_single_index(
    request: Option<&str>,
    prefix: &str,
    default: usize,
) -> Option<usize> {
    parse_request_name(request, prefix, default, |s| s.parse().ok())
}
