use anyhow::Result as Fallible;
use anyhow::{format_err, Context};
use futures::stream::{FuturesOrdered, StreamExt};
use lazy_static::lazy_static;
use reqwest::{Client, ClientBuilder};
use semver::Version;
use std::collections::HashSet;
use std::ops::Range;
use std::str::FromStr;
use std::time::Duration;
use url::Url;

use cincinnati::plugins::prelude_plugin_impl::TryFutureExt;
use cincinnati::Release;
// base url for signature storage - see https://github.com/openshift/cluster-update-keys/blob/master/stores/store-openshift-official-release-mirror
lazy_static! {
  static ref BASE_URL: Url =
    Url::parse("https://mirror.openshift.com/pub/openshift-v4/signatures/openshift/release/")
      .expect("could not parse url");
}

static DEFAULT_TIMEOUT_SECS: u64 = 30;

// CVO has maxSignatureSearch = 10 in pkg/verify/verify.go
static MAX_SIGNATURES: u64 = 10;

fn payload_from_release(release: &Release) -> Fallible<String> {
  match release {
    Release::Concrete(c) => Ok(c.payload.clone()),
    _ => Err(format_err!("not a concrete release")),
  }
}

async fn fetch_url(client: &Client, sha: &str, i: u64) -> Fallible<()> {
  let url = BASE_URL
    .join(format!("{}/", sha.replace(":", "=")).as_str())?
    .join(format!("signature-{}", i).as_str())?;
  let res = client
    .get(url.clone())
    .send()
    .map_err(|e| anyhow::anyhow!(e.to_string()))
    .await?;

  let url_s = url.to_string();
  let status = res.status();
  match status.is_success() {
    true => Ok(()),
    false => Err(format_err!("Error fetching {} - {}", url_s, status)),
  }
}

async fn find_signatures_for_version(client: &Client, release: &Release) -> Fallible<()> {
  let mut errors = vec![];
  let payload = payload_from_release(release)?;
  let digest = payload
    .split("@")
    .last()
    .ok_or_else(|| format_err!("could not parse payload '{:?}'", payload))?;

  let mut attempts = Range {
    start: 1,
    end: MAX_SIGNATURES,
  };
  loop {
    if let Some(i) = attempts.next() {
      match fetch_url(client, digest, i).await {
        Ok(_) => return Ok(()),
        Err(e) => errors.push(e),
      }
    } else {
      return Err(format_err!(
        "Failed to find signatures for {} - {}: {:#?}",
        release.version(),
        payload,
        errors
      ));
    }
  }
}

fn is_release_in_versions(versions: &HashSet<Version>, release: &Release) -> bool {
  // Strip arch identifier
  let stripped_version = release
    .version()
    .split("+")
    .next()
    .ok_or(release.version())
    .unwrap();
  let version = Version::from_str(stripped_version).unwrap();
  versions.contains(&version)
}

pub async fn run(
  releases: &Vec<Release>,
  found_versions: &HashSet<semver::Version>,
) -> Fallible<()> {
  println!("Checking release signatures");

  let client: Client = ClientBuilder::new()
    .gzip(true)
    .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
    .build()
    .context("Building reqwest client")?;

  // Filter scraped images - skip CI images
  let tracked_versions: Vec<&cincinnati::Release> = releases
    .into_iter()
    .filter(|ref r| is_release_in_versions(found_versions, &r))
    .collect::<Vec<&cincinnati::Release>>();

  let results: Vec<Fallible<()>> = tracked_versions
    //Attempt to find signatures for filtered releases
    .into_iter()
    .map(|ref r| find_signatures_for_version(&client, r))
    .collect::<FuturesOrdered<_>>()
    .collect::<Vec<Fallible<()>>>()
    .await
    // Filter to keep errors only
    .into_iter()
    .filter(|e| e.is_err())
    .collect();
  if results.is_empty() {
    Ok(())
  } else {
    Err(format_err!("Signature check errors: {:#?}", results))
  }
}
