// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::str::FromStr;

use percent_encoding::{percent_decode, percent_encode, AsciiSet, CONTROLS};
use rusoto_core::Region;
use url::Url;

#[derive(Clone)]
pub struct GstS3Url {
    pub region: Region,
    pub bucket: String,
    pub object: String,
    pub version: Option<String>,
}

// FIXME: Copied from the url crate, see https://github.com/servo/rust-url/issues/529
// https://url.spec.whatwg.org/#fragment-percent-encode-set
const FRAGMENT: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'<').add(b'>').add(b'`');
// https://url.spec.whatwg.org/#path-percent-encode-set
const PATH: &AsciiSet = &FRAGMENT.add(b'#').add(b'?').add(b'{').add(b'}');
const PATH_SEGMENT: &AsciiSet = &PATH.add(b'/').add(b'%');

impl ToString for GstS3Url {
    fn to_string(&self) -> String {
        format!(
            "s3://{}/{}/{}{}",
            self.region.name(),
            self.bucket,
            percent_encode(self.object.as_bytes(), PATH_SEGMENT),
            if self.version.is_some() {
                format!("?version={}", self.version.clone().unwrap())
            } else {
                "".to_string()
            }
        )
    }
}

pub fn parse_s3_url(url_str: &str) -> Result<GstS3Url, String> {
    let url = Url::parse(url_str).map_err(|err| format!("Parse error: {}", err))?;

    if url.scheme() != "s3" {
        return Err(format!("Unsupported URI '{}'", url.scheme()));
    }

    if !url.has_host() {
        return Err(format!("Invalid host in uri '{}'", url));
    }

    let host = url.host_str().unwrap();
    let region = Region::from_str(host).map_err(|_| format!("Invalid region '{}'", host))?;

    let mut path = url
        .path_segments()
        .ok_or_else(|| format!("Invalid uri '{}'", url))?;

    let bucket = path.next().unwrap().to_string();

    let o = path
        .next()
        .ok_or_else(|| format!("Invalid empty object/bucket '{}'", url))?;

    let mut object = percent_decode(o.as_bytes())
        .decode_utf8()
        .unwrap()
        .to_string();
    if o.is_empty() {
        return Err(format!("Invalid empty object/bucket '{}'", url));
    }

    object = path.fold(object, |o, p| format!("{}/{}", o, p));

    let mut q = url.query_pairs();
    let v = q.next();
    let version;

    match v {
        Some((ref k, ref v)) if k == "version" => version = Some((*v).to_string()),
        None => version = None,
        Some(_) => return Err("Bad query, only 'version' is supported".to_owned()),
    }

    if q.next() != None {
        return Err("Extra query terms, only 'version' is supported".to_owned());
    }

    Ok(GstS3Url {
        region,
        bucket,
        object,
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cannot_be_base() {
        assert!(parse_s3_url("data:something").is_err());
    }

    #[test]
    fn invalid_scheme() {
        assert!(parse_s3_url("file:///dev/zero").is_err());
    }

    #[test]
    fn bad_region() {
        assert!(parse_s3_url("s3://atlantis-1/i-hope-we/dont-find-this").is_err());
    }

    #[test]
    fn no_bucket() {
        assert!(parse_s3_url("s3://ap-south-1").is_err());
        assert!(parse_s3_url("s3://ap-south-1/").is_err());
    }

    #[test]
    fn no_object() {
        assert!(parse_s3_url("s3://ap-south-1/my-bucket").is_err());
        assert!(parse_s3_url("s3://ap-south-1/my-bucket/").is_err());
    }

    #[test]
    fn valid_simple() {
        assert!(parse_s3_url("s3://ap-south-1/my-bucket/my-object").is_ok());
    }

    #[test]
    fn extraneous_query() {
        assert!(parse_s3_url("s3://ap-south-1/my-bucket/my-object?foo=bar").is_err());
    }

    #[test]
    fn valid_version() {
        assert!(parse_s3_url("s3://ap-south-1/my-bucket/my-object?version=one").is_ok());
    }

    #[test]
    fn trailing_slash() {
        // Slashes are valid at the end of the object key
        assert_eq!(
            parse_s3_url("s3://ap-south-1/my-bucket/my-object/")
                .unwrap()
                .object,
            "my-object/"
        );
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(
            parse_s3_url("s3://ap-south-1/my-bucket/my%20object")
                .unwrap()
                .object,
            "my object"
        );
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(
            parse_s3_url("s3://ap-south-1/my-bucket/my object")
                .unwrap()
                .to_string(),
            "s3://ap-south-1/my-bucket/my%20object"
        );
    }
}
