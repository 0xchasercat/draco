//! Shared offline [`ScriptFetcher`] doubles for the integration tests.
//!
//! std-only (the crate's dev-dependencies are deliberately empty): the trait's
//! `LocalBoxFuture` return type is spelled as its underlying
//! `Pin<Box<dyn Future>>` so no `futures` dev-dep is needed.
#![allow(dead_code)] // each test binary compiles this module independently

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use draco_runtime::{ScriptFetcher, SharedSource};

type BoxedFetch<'a> = Pin<Box<dyn Future<Output = Option<SharedSource>> + 'a>>;

/// Fetcher over a fixed `{ url -> bytes }` map (the offline stand-in for the
/// production net+cache fetcher).
pub struct MapFetcher(pub HashMap<String, SharedSource>);

impl ScriptFetcher for MapFetcher {
    fn fetch<'a>(&'a self, url: &'a str) -> BoxedFetch<'a> {
        let hit = self.0.get(url).map(Arc::clone);
        Box::pin(async move { hit })
    }
}

/// A fetcher that resolves nothing — for pages with no external code.
pub fn null_fetcher() -> Rc<dyn ScriptFetcher> {
    Rc::new(MapFetcher(HashMap::new()))
}

/// A fetcher serving a fixed `{ url -> bytes }` set.
pub fn map_fetcher(entries: HashMap<String, Vec<u8>>) -> Rc<dyn ScriptFetcher> {
    Rc::new(MapFetcher(
        entries
            .into_iter()
            .map(|(url, bytes)| (url, bytes.into()))
            .collect(),
    ))
}

/// A fetcher backed by a closure — ports the old `ScriptLoader` test doubles.
pub struct FnFetcher<F: Fn(&str) -> Option<Vec<u8>>>(pub F);

impl<F: Fn(&str) -> Option<Vec<u8>>> ScriptFetcher for FnFetcher<F> {
    fn fetch<'a>(&'a self, url: &'a str) -> BoxedFetch<'a> {
        let hit = (self.0)(url).map(SharedSource::from);
        Box::pin(async move { hit })
    }
}

/// Wrap a closure as a shared fetcher.
pub fn fn_fetcher(f: impl Fn(&str) -> Option<Vec<u8>> + 'static) -> Rc<dyn ScriptFetcher> {
    Rc::new(FnFetcher(f))
}
