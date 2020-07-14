use crate::deploy::ContainerVolume;
use crate::error::Error;
use crate::message::Shutdown;
use crate::util::path::{CachePath, ProjectedPath};
use crate::util::url::TransferUrl;
use crate::util::Abort;
use crate::{ExeUnitContext, Result};
use actix::prelude::*;
use futures::future::{AbortHandle, Abortable};
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;
use ya_transfer::error::Error as TransferError;
use ya_transfer::{
    transfer, FileTransferProvider, GftpTransferProvider, HashStream, HttpTransferProvider,
    TransferData, TransferProvider, TransferSink, TransferStream,
};

#[derive(Clone, Debug, Message)]
#[rtype(result = "Result<()>")]
pub struct TransferResource {
    pub from: String,
    pub to: String,
}

#[derive(Message)]
#[rtype(result = "Result<()>")]
pub struct AddVolumes(Vec<ContainerVolume>);

impl AddVolumes {
    pub fn new(vols: Vec<ContainerVolume>) -> Self {
        AddVolumes(vols)
    }
}

#[derive(Clone, Debug, Message)]
#[rtype(result = "Result<PathBuf>")]
pub struct DeployImage;

#[derive(Clone, Debug, Message)]
#[rtype(result = "()")]
pub struct AbortTransfers;

#[derive(Clone, Debug, Message)]
#[rtype("()")]
struct AddAbortHandle(Abort);

#[derive(Clone, Debug, Message)]
#[rtype("()")]
struct RemoveAbortHandle(Abort);

struct ContainerTransferProvider {
    file_tp: FileTransferProvider,
    work_dir: PathBuf,
    vols: Vec<ContainerVolume>,
}

impl ContainerTransferProvider {
    fn new(work_dir: PathBuf, vols: Vec<ContainerVolume>) -> Self {
        let file_tp = Default::default();
        ContainerTransferProvider {
            file_tp,
            work_dir,
            vols,
        }
    }

    fn resolve_path(&self, container_path: &str) -> std::result::Result<PathBuf, TransferError> {
        fn is_prefix_of(base: &str, path: &str) -> usize {
            if path.starts_with(base) && (path == base || path[base.len()..].starts_with("/")) {
                base.len() + 1
            } else {
                0
            }
        }

        if let Some((_, c)) = self
            .vols
            .iter()
            .map(|c| (is_prefix_of(&c.path, container_path), c))
            .max_by_key(|(prefix, _)| *prefix)
            .filter(|(prefix, _)| (*prefix) > 0)
        {
            let vol_base = self.work_dir.join(&c.name);

            if c.path == container_path {
                return Ok(vol_base);
            }

            let path = &container_path[c.path.len() + 1..];
            if path.starts_with("/") {
                return Err(TransferError::IoError(io::Error::new(
                    io::ErrorKind::NotFound,
                    anyhow::anyhow!("invalid path format: [{}]", container_path),
                )));
            }
            Ok(vol_base.join(path))
        } else {
            log::warn!("not found!!");
            Err(TransferError::IoError(io::Error::new(
                io::ErrorKind::NotFound,
                anyhow::anyhow!("path {} not found in container", container_path),
            )))
        }
    }

    fn resolve_url(&self, path: &str) -> std::result::Result<Url, TransferError> {
        Ok(Url::from_file_path(self.resolve_path(path)?).unwrap())
    }
}

impl TransferProvider<TransferData, TransferError> for ContainerTransferProvider {
    fn schemes(&self) -> Vec<&'static str> {
        vec!["container"]
    }

    fn source(&self, url: &Url) -> TransferStream<TransferData, TransferError> {
        let file_url = match self.resolve_url(url.path()) {
            Ok(v) => v,
            Err(e) => return TransferStream::err(e),
        };
        self.file_tp.source(&file_url)
    }

    fn destination(&self, url: &Url) -> TransferSink<TransferData, TransferError> {
        let file_url = match self.resolve_url(url.path()) {
            Ok(v) => v,
            Err(e) => return TransferSink::err(e),
        };
        self.file_tp.destination(&file_url)
    }
}

/// Handles resources transfers.
pub struct TransferService {
    providers: HashMap<&'static str, Rc<dyn TransferProvider<TransferData, TransferError>>>,
    cache: Cache,
    work_dir: PathBuf,
    task_package: String,
    abort_handles: HashSet<Abort>,
}

impl TransferService {
    pub fn new(ctx: &ExeUnitContext) -> TransferService {
        let mut providers = HashMap::new();

        let provider_vec: Vec<Rc<dyn TransferProvider<TransferData, TransferError>>> = vec![
            Rc::new(GftpTransferProvider::default()),
            Rc::new(HttpTransferProvider::default()),
        ];
        for provider in provider_vec {
            for scheme in provider.schemes() {
                providers.insert(scheme, provider.clone());
            }
        }

        TransferService {
            providers,
            cache: Cache::new(ctx.cache_dir.clone()),
            work_dir: ctx.work_dir.clone(),
            task_package: ctx.agreement.task_package.clone(),
            abort_handles: HashSet::new(),
        }
    }

    fn source(
        &self,
        transfer_url: &TransferUrl,
    ) -> Result<Box<dyn Stream<Item = std::result::Result<TransferData, TransferError>> + Unpin>>
    {
        let scheme = transfer_url.url.scheme();
        let provider = self
            .providers
            .get(scheme)
            .ok_or(TransferError::UnsupportedSchemeError(scheme.to_owned()))?;

        let stream = provider.source(&transfer_url.url);
        match &transfer_url.hash {
            Some(hash) => Ok(Box::new(HashStream::try_new(
                stream,
                &hash.alg,
                hash.val.clone(),
            )?)),
            None => Ok(Box::new(stream)),
        }
    }

    fn destination(
        &self,
        transfer_url: &TransferUrl,
    ) -> Result<TransferSink<TransferData, TransferError>> {
        let scheme = transfer_url.url.scheme();

        let provider = self
            .providers
            .get(scheme)
            .ok_or(TransferError::UnsupportedSchemeError(scheme.to_owned()))?;

        Ok(provider.destination(&transfer_url.url))
    }
}

impl Actor for TransferService {
    type Context = Context<Self>;

    fn started(&mut self, _: &mut Self::Context) {
        log::info!("Transfer service started");
    }

    fn stopped(&mut self, _: &mut Self::Context) {
        log::info!("Transfer service stopped");
    }
}

macro_rules! actor_try {
    ($expr:expr) => {
        match $expr {
            Ok(val) => val,
            Err(err) => {
                return ActorResponse::reply(Err(Error::from(err)));
            }
        }
    };
    ($expr:expr,) => {
        $crate::actor_try!($expr)
    };
}

impl Handler<DeployImage> for TransferService {
    type Result = ActorResponse<Self, PathBuf, Error>;

    fn handle(&mut self, _: DeployImage, ctx: &mut Self::Context) -> Self::Result {
        let file_provider: FileTransferProvider = Default::default();
        let source_url = actor_try!(TransferUrl::parse_with_hash(&self.task_package, "file"));
        let cache_name = actor_try!(Cache::name(&source_url));
        let temp_path = self.cache.to_temp_path(&cache_name);
        let cache_path = self.cache.to_cache_path(&cache_name);
        let final_path = self.cache.to_final_path(&cache_name);
        let temp_url = Url::from_file_path(temp_path.to_path_buf()).unwrap();

        log::info!(
            "Deploying from {:?} to {:?}",
            source_url.url,
            final_path.to_path_buf()
        );

        let source = actor_try!(self.source(&source_url));
        let dest = file_provider.destination(&temp_url);

        let address = ctx.address();
        let (handle, reg) = AbortHandle::new_pair();
        let abort = Abort::from(handle);

        let fut = async move {
            let final_path = final_path.to_path_buf();
            let temp_path = temp_path.to_path_buf();
            let cache_path = cache_path.to_path_buf();

            if cache_path.exists() {
                log::info!("Deploying cached image: {:?}", cache_path);
                std::fs::copy(cache_path, &final_path)?;
                return Ok(final_path);
            }

            address.send(AddAbortHandle(abort.clone())).await?;
            Abortable::new(transfer(source, dest), reg)
                .await
                .map_err(TransferError::from)??;
            address.send(RemoveAbortHandle(abort)).await?;

            std::fs::rename(temp_path, &cache_path)?;
            std::fs::copy(cache_path, &final_path)?;

            log::info!("Deployment from {:?} finished", source_url.url);
            Ok(final_path)
        };

        return ActorResponse::r#async(fut.into_actor(self));
    }
}

impl Handler<TransferResource> for TransferService {
    type Result = ActorResponse<Self, (), Error>;

    fn handle(&mut self, msg: TransferResource, ctx: &mut Self::Context) -> Self::Result {
        let address = ctx.address();
        let from = actor_try!(TransferUrl::parse(&msg.from, "container"));
        let to = actor_try!(TransferUrl::parse(&msg.to, "container"));

        log::info!("Transferring {:?} to {:?}", from.url, to.url);

        let source = actor_try!(self.source(&from));
        let dest = actor_try!(self.destination(&to));

        let (handle, reg) = AbortHandle::new_pair();
        let abort = Abort::from(handle);

        return ActorResponse::r#async(
            async move {
                address.send(AddAbortHandle(abort.clone())).await?;
                Abortable::new(transfer(source, dest), reg)
                    .await
                    .map_err(TransferError::from)??;
                address.send(RemoveAbortHandle(abort)).await?;
                log::info!("Transfer of {:?} to {:?} finished", from.url, to.url);
                Ok(())
            }
            .into_actor(self),
        );
    }
}

impl Handler<AddAbortHandle> for TransferService {
    type Result = <AddAbortHandle as Message>::Result;

    fn handle(&mut self, msg: AddAbortHandle, _: &mut Self::Context) -> Self::Result {
        self.abort_handles.insert(msg.0);
    }
}

impl Handler<RemoveAbortHandle> for TransferService {
    type Result = <RemoveAbortHandle as Message>::Result;

    fn handle(&mut self, msg: RemoveAbortHandle, _: &mut Self::Context) -> Self::Result {
        self.abort_handles.remove(&msg.0);
    }
}

impl Handler<AbortTransfers> for TransferService {
    type Result = <AbortTransfers as Message>::Result;

    fn handle(&mut self, _: AbortTransfers, _: &mut Self::Context) -> Self::Result {
        for handle in std::mem::replace(&mut self.abort_handles, HashSet::new()).into_iter() {
            handle.abort();
        }
    }
}

impl Handler<Shutdown> for TransferService {
    type Result = <Shutdown as Message>::Result;

    fn handle(&mut self, _: Shutdown, ctx: &mut Self::Context) -> Self::Result {
        ctx.address().do_send(AbortTransfers {});
        ctx.stop();
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Cache {
    dir: PathBuf,
    tmp_dir: PathBuf,
}

impl Cache {
    fn new(dir: PathBuf) -> Self {
        let tmp_dir = dir.clone().join("tmp");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        Cache { dir, tmp_dir }
    }

    fn name(transfer_url: &TransferUrl) -> Result<CachePath> {
        let hash = match &transfer_url.hash {
            Some(hash) => hash,
            None => return Err(TransferError::InvalidUrlError("hash required".to_owned()).into()),
        };

        let name = transfer_url.file_name();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string();

        Ok(CachePath::new(name.into(), hash.val.clone(), nonce))
    }

    #[inline(always)]
    fn to_temp_path(&self, path: &CachePath) -> ProjectedPath {
        ProjectedPath::local(self.tmp_dir.clone(), path.temp_path_buf())
    }

    #[inline(always)]
    fn to_cache_path(&self, path: &CachePath) -> ProjectedPath {
        ProjectedPath::local(self.tmp_dir.clone(), path.cache_path_buf())
    }

    #[inline(always)]
    fn to_final_path(&self, path: &CachePath) -> ProjectedPath {
        ProjectedPath::local(self.dir.clone(), path.final_path_buf())
    }
}

impl TryFrom<ProjectedPath> for TransferUrl {
    type Error = Error;

    fn try_from(value: ProjectedPath) -> Result<Self> {
        TransferUrl::parse(
            value
                .to_path_buf()
                .to_str()
                .ok_or(Error::local(TransferError::InvalidUrlError(
                    "Invalid path".to_owned(),
                )))?,
            "file",
        )
        .map_err(Error::local)
    }
}

impl Handler<AddVolumes> for TransferService {
    type Result = Result<()>;

    fn handle(&mut self, msg: AddVolumes, _ctx: &mut Self::Context) -> Self::Result {
        let container_transfer_provider =
            ContainerTransferProvider::new(self.work_dir.clone(), msg.0);
        self.providers
            .insert("container", Rc::new(container_transfer_provider));
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_resolve_1() {
        let c = ContainerTransferProvider::new(
            "/tmp".into(),
            vec![
                ContainerVolume {
                    name: "vol-3a9710d2-42f1-4502-9098-bc0bab9e7acc".into(),
                    path: "/in".into(),
                },
                ContainerVolume {
                    name: "vol-17599e4b-3aab-4fa8-b08d-440f48bd61e9".into(),
                    path: "/out".into(),
                },
            ],
        );
        assert_eq!(
            c.resolve_path("/in/task.json").unwrap(),
            std::path::Path::new("/tmp/vol-3a9710d2-42f1-4502-9098-bc0bab9e7acc/task.json")
        );
        assert_eq!(
            c.resolve_path("/out/task.json").unwrap(),
            std::path::Path::new("/tmp/vol-17599e4b-3aab-4fa8-b08d-440f48bd61e9/task.json")
        );
        assert!(c.resolve_path("/outs/task.json").is_err());
        assert!(c.resolve_path("/in//task.json").is_err());
        assert_eq!(
            c.resolve_path("/in").unwrap(),
            std::path::Path::new("/tmp/vol-3a9710d2-42f1-4502-9098-bc0bab9e7acc")
        );
    }

    #[test]
    fn test_resolve_2() {
        let c = ContainerTransferProvider::new(
            "/tmp".into(),
            vec![
                ContainerVolume {
                    name: "vol-1".into(),
                    path: "/in/dst".into(),
                },
                ContainerVolume {
                    name: "vol-2".into(),
                    path: "/in".into(),
                },
                ContainerVolume {
                    name: "vol-3".into(),
                    path: "/out".into(),
                },
                ContainerVolume {
                    name: "vol-4".into(),
                    path: "/out/bin".into(),
                },
                ContainerVolume {
                    name: "vol-5".into(),
                    path: "/out/lib".into(),
                },
            ],
        );

        let check_resolve = |container_path, expected_result| {
            assert_eq!(
                c.resolve_path(container_path).unwrap(),
                Path::new(expected_result)
            )
        };

        check_resolve("/in/task.json", "/tmp/vol-2/task.json");
        check_resolve("/in/dst/smok.bin", "/tmp/vol-1/smok.bin");
        check_resolve("/out/b/x.png", "/tmp/vol-3/b/x.png");
        check_resolve("/out/bin/bash", "/tmp/vol-4/bash");
        check_resolve("/out/lib/libc.so", "/tmp/vol-5/libc.so");
    }

    // [ContainerVolume { name: "", path: "" }, ContainerVolume { name: "", path: "" }, ContainerVo
    //        │ lume { name: "", path: "" }]
    #[test]
    fn test_resolve_3() {
        let c = ContainerTransferProvider::new(
            "/tmp".into(),
            vec![
                ContainerVolume {
                    name: "vol-bd959639-9148-4d7c-8ba2-05a654e84476".into(),
                    path: "/golem/output".into(),
                },
                ContainerVolume {
                    name: "vol-4d59d1d6-2571-4ab8-a86a-b6199a9a1f4b".into(),
                    path: "/golem/resource".into(),
                },
                ContainerVolume {
                    name: "vol-b51194da-2fce-45b7-bff8-37e4ef8f7535".into(),
                    path: "/golem/work".into(),
                },
            ],
        );

        let check_resolve = |container_path, expected_result| {
            assert_eq!(
                c.resolve_path(container_path).unwrap(),
                Path::new(expected_result)
            )
        };

        check_resolve(
            "/golem/resource/scene.blend",
            "/tmp/vol-4d59d1d6-2571-4ab8-a86a-b6199a9a1f4b/scene.blend",
        );
    }

    #[test]
    fn test_resolve_compat() {
        let c = ContainerTransferProvider::new(
            "/tmp".into(),
            vec![ContainerVolume {
                name: ".".into(),
                path: "".into(),
            }],
        );
        eprintln!("{}", c.resolve_path("/in/tasks.json").unwrap().display());
    }
}
