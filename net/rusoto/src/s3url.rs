// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

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
            match self.region {
                Region::Custom {
                    ref name,
                    ref endpoint,
                } => {
                    format!(
                        "{}+{}",
                        base32::encode(
                            base32::Alphabet::RFC4648 { padding: true },
                            name.as_bytes(),
                        ),
                        base32::encode(
                            base32::Alphabet::RFC4648 { padding: true },
                            endpoint.as_bytes(),
                        ),
                    )
                }
                _ => {
                    String::from(self.region.name())
                }
            },
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
    let region = host
        .parse::<Region>()
        .or_else(|_| {
            let (name, endpoint) = host.split_once('+').ok_or(())?;
            let name =
                base32::decode(base32::Alphabet::RFC4648 { padding: true }, name).ok_or(())?;
            let endpoint =
                base32::decode(base32::Alphabet::RFC4648 { padding: true }, endpoint).ok_or(())?;
            let name = String::from_utf8(name).map_err(|_| ())?;
            let endpoint = String::from_utf8(endpoint).map_err(|_| ())?;
            Ok(Region::Custom { name, endpoint })
        })
        .map_err(|_: ()| format!("Invalid region '{}'", host))?;

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

    let version = match v {
        Some((ref k, ref v)) if k == "version" => Some((*v).to_string()),
        None => None,
        Some(_) => return Err("Bad query, only 'version' is supported".to_owned()),
    };

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
