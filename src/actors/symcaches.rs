use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};

use actix::{Actor, Addr, Context, Handler, Message, ResponseFuture};

use failure::{Fail, ResultExt};

use futures::{future::Future, lazy};

use symbolic::{common::ByteView, symcache};

use tokio_threadpool::ThreadPool;

use crate::futures::measure_task;

use crate::{
    actors::{
        cache::{CacheActor, CacheItemRequest, CacheKey, ComputeMemoized},
        objects::{FetchObject, ObjectsActor},
    },
    types::{FileType, ObjectId, ObjectType, Scope, SourceConfig},
};

#[derive(Fail, Debug, Clone, Copy)]
pub enum SymCacheErrorKind {
    #[fail(display = "failed to download")]
    Io,

    #[fail(display = "failed to download object")]
    Fetching,

    #[fail(display = "failed sending message to objects actor")]
    Mailbox,

    #[fail(display = "failed to parse symcache")]
    Parsing,

    #[fail(display = "failed to parse object")]
    ObjectParsing,

    #[fail(display = "symcache building took too long")]
    Timeout,
}

symbolic::common::derive_failure!(
    SymCacheError,
    SymCacheErrorKind,
    doc = "Errors happening while generating a symcache"
);

impl From<io::Error> for SymCacheError {
    fn from(e: io::Error) -> Self {
        e.context(SymCacheErrorKind::Io).into()
    }
}

pub struct SymCacheActor {
    symcaches: Addr<CacheActor<FetchSymCacheInternal>>,
    objects: Addr<ObjectsActor>,
    threadpool: Arc<ThreadPool>,
}

impl Actor for SymCacheActor {
    type Context = Context<Self>;
}

impl SymCacheActor {
    pub fn new(
        symcaches: Addr<CacheActor<FetchSymCacheInternal>>,
        objects: Addr<ObjectsActor>,
        threadpool: Arc<ThreadPool>,
    ) -> Self {
        SymCacheActor {
            symcaches,
            objects,
            threadpool,
        }
    }
}

#[derive(Clone)]
pub struct SymCache {
    inner: Option<ByteView<'static>>,
    scope: Scope,
    request: FetchSymCacheInternal,
}

impl SymCache {
    pub fn get_symcache(&self) -> Result<Option<symcache::SymCache<'_>>, SymCacheError> {
        let bytes = match self.inner {
            Some(ref x) => x,
            None => return Ok(None),
        };

        if &bytes[..] == b"malformed" {
            return Err(SymCacheErrorKind::ObjectParsing.into());
        }

        Ok(Some(
            symcache::SymCache::parse(bytes).context(SymCacheErrorKind::Parsing)?,
        ))
    }
}

#[derive(Clone)]
pub struct FetchSymCacheInternal {
    request: FetchSymCache,
    objects: Addr<ObjectsActor>,
    threadpool: Arc<ThreadPool>,
}

impl CacheItemRequest for FetchSymCacheInternal {
    type Item = SymCache;
    type Error = SymCacheError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: self.request.identifier.get_cache_key(),
            scope: self.request.scope.clone(),
        }
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = Scope, Error = Self::Error>> {
        let objects = self.objects.clone();

        let path = path.to_owned();
        let threadpool = self.threadpool.clone();

        // TODO: Backoff + retry when download is interrupted? Or should we just have retry logic
        // in Sentry itself?
        let result = objects
            .send(FetchObject {
                filetypes: FileType::from_object_type(&self.request.object_type),
                identifier: self.request.identifier.clone(),
                sources: self.request.sources.clone(),
                scope: self.request.scope.clone(),
            })
            .map_err(|e| e.context(SymCacheErrorKind::Mailbox).into())
            .and_then(move |result| {
                threadpool.spawn_handle(lazy(move || {
                    let object = result.context(SymCacheErrorKind::Fetching)?;
                    let mut file =
                        BufWriter::new(File::create(&path).context(SymCacheErrorKind::Io)?);
                    match object.get_object() {
                        Ok(Some(object)) => {
                            let _file = symcache::SymCacheWriter::write_object(&object, file)
                                .context(SymCacheErrorKind::Io)?;
                        }
                        Ok(None) => (),
                        Err(_) => {
                            file.write_all(b"malformed")
                                .context(SymCacheErrorKind::Io)?;
                        }
                    };

                    Ok(object.scope().clone())
                }))
            });

        Box::new(measure_task(
            "fetch_symcache",
            Some((Duration::from_secs(300), || {
                SymCacheErrorKind::Timeout.into()
            })),
            result,
        ))
    }

    fn load(self, scope: Scope, data: ByteView<'static>) -> Result<Self::Item, Self::Error> {
        Ok(SymCache {
            request: self,
            scope,
            inner: if !data.is_empty() { Some(data) } else { None },
        })
    }
}

/// Information for fetching the symbols for this symcache
#[derive(Debug, Clone)]
pub struct FetchSymCache {
    pub object_type: ObjectType,
    pub identifier: ObjectId,
    pub sources: Vec<SourceConfig>,
    pub scope: Scope,
}

impl Message for FetchSymCache {
    type Result = Result<Arc<SymCache>, Arc<SymCacheError>>;
}

impl Handler<FetchSymCache> for SymCacheActor {
    type Result = ResponseFuture<Arc<SymCache>, Arc<SymCacheError>>;

    fn handle(&mut self, request: FetchSymCache, _ctx: &mut Self::Context) -> Self::Result {
        Box::new(
            self.symcaches
                .send(ComputeMemoized(FetchSymCacheInternal {
                    request,
                    objects: self.objects.clone(),
                    threadpool: self.threadpool.clone(),
                }))
                .map_err(|e| Arc::new(e.context(SymCacheErrorKind::Mailbox).into()))
                .and_then(|response| Ok(response?)),
        )
    }
}
