use std::{rc::Rc, sync::Arc, time::SystemTime};

use deno_cache_dir::file_fetcher::{
  AuthTokens, BlobData, CacheSetting, FetchNoFollowOptions, FileFetcher, FileFetcherOptions,
  HeaderMap, NullBlobStore, NullMemoryFiles, SendError, SendResponse,
};
use deno_config::workspace::{WorkspaceDirectory, WorkspaceDiscoverOptions};
use deno_graph::MediaType;
use deno_resolver::{
  DefaultDenoResolverRc, DenoResolver, DenoResolverOptions, NodeAndNpmReqResolver,
  factory::{ResolverFactory, WorkspaceFactory},
  npm::{ByonmNpmResolverCreateOptions, NpmReqResolver, NpmReqResolverOptions},
  workspace::ResolutionKind,
};
use node_resolver::{
  ConditionsFromResolutionMode, IsBuiltInNodeModuleChecker, NodeResolver, PackageJsonResolver,
};
use reqwest::Response;
use rolldown::{
  Bundler, BundlerOptions, ChunkFilenamesOutputOption, InputItem, ModuleType, SourceMapType,
};
use rolldown_common::ImportKind;
use rolldown_plugin::{HookResolveIdOutput, Plugin};
use rolldown_testing::workspace;
use sugar_path::SugarPath;
use sys_traits::{EnvCurrentDir, impls::RealSys};
use url::Url;

#[derive(Debug)]
struct HttpImportPlugin {
  resolver: DefaultDenoResolverRc<RealSys>,
  file_fetcher: Arc<FileFetcher<NullBlobStore, RealSys, RolldownHttpClient>>,
}

impl Plugin for HttpImportPlugin {
  fn name(&self) -> std::borrow::Cow<'static, str> {
    "http-import".into()
  }

  async fn resolve_id(
    &self,
    _ctx: &rolldown_plugin::PluginContext,
    args: &rolldown_plugin::HookResolveIdArgs<'_>,
  ) -> rolldown_plugin::HookResolveIdReturn {
    let referrer = match args.importer {
      Some(importer) => Url::parse(importer)?,
      None => deno_path_util::url_from_directory_path(&std::env::current_dir()?)?,
    };
    let resolved = self.resolver.resolve(
      args.specifier,
      &referrer,
      match args.kind {
        ImportKind::Require => node_resolver::ResolutionMode::Require,
        _ => node_resolver::ResolutionMode::Import,
      },
      node_resolver::NodeResolutionKind::Execution,
    )?;
    Ok(Some(HookResolveIdOutput { id: resolved.url.to_string().into(), ..Default::default() }))
  }

  #[allow(clippy::print_stdout)]
  async fn load(
    &self,
    _ctx: &rolldown_plugin::PluginContext,
    args: &rolldown_plugin::HookLoadArgs<'_>,
  ) -> rolldown_plugin::HookLoadReturn {
    println!("Downloading: {}", args.id);
    let url = Url::parse(args.id)?;
    let file_fetcher = self.file_fetcher.clone();

    tokio::task::spawn_blocking(|| {
      // super inefficient...
      let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
      rt.block_on(async move {
        let file_or_redirect = file_fetcher
          .fetch_no_follow(
            &url,
            FetchNoFollowOptions {
              maybe_auth: None,
              maybe_checksum: None,
              maybe_accept: None,
              maybe_cache_setting: None,
            },
          )
          .await?;
        match file_or_redirect {
          deno_cache_dir::file_fetcher::FileOrRedirect::File(file) => {
            Ok(Some(rolldown_plugin::HookLoadOutput {
              code: String::from_utf8_lossy(&file.source).to_string(),
              module_type: Some(
                match MediaType::from_specifier_and_headers(&url, file.maybe_headers.as_ref()) {
                  MediaType::JavaScript | MediaType::Mjs | MediaType::Cjs => ModuleType::Js,
                  MediaType::Jsx => ModuleType::Jsx,
                  MediaType::TypeScript
                  | MediaType::Mts
                  | MediaType::Cts
                  | MediaType::Dts
                  | MediaType::Dmts
                  | MediaType::Dcts => ModuleType::Ts,
                  MediaType::Tsx => ModuleType::Tsx,
                  MediaType::Css => ModuleType::Css,
                  MediaType::Json => ModuleType::Json,
                  MediaType::Wasm => ModuleType::Custom("wasm".to_string()),
                  MediaType::SourceMap => ModuleType::Custom("map".to_string()),
                  MediaType::Unknown => ModuleType::Asset,
                  MediaType::Html => ModuleType::Custom("html".to_string()),
                  MediaType::Sql => ModuleType::Asset,
                },
              ),
              ..Default::default()
            }))
          }
          deno_cache_dir::file_fetcher::FileOrRedirect::Redirect(url) => todo!(),
        }
      })
    })
    .await
    .unwrap()
  }
}

#[derive(Debug)]
struct RolldownHttpClient {
  client: reqwest::Client,
}

#[async_trait::async_trait(?Send)]
impl deno_cache_dir::file_fetcher::HttpClient for RolldownHttpClient {
  async fn send_no_follow(&self, url: &Url, headers: HeaderMap) -> Result<SendResponse, SendError> {
    let response = self
      .client
      .get(url.clone())
      .headers(headers)
      .send()
      .await
      .map_err(|err| SendError::Failed(Box::new(err)))?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
      Ok(SendResponse::NotModified)
    } else if response.status().is_redirection() {
      // todo: how to not clone?
      let headers = response.headers().clone();
      Ok(SendResponse::Redirect(headers))
    } else if response.status() == reqwest::StatusCode::NOT_FOUND {
      Err(SendError::NotFound)
    } else if response.status().is_server_error() {
      Err(SendError::StatusCode(response.status()))
    } else {
      // todo: how to not clone?
      let headers = response.headers().clone();
      let bytes = response.bytes().await.map_err(|err| SendError::Failed(Box::new(err)))?;
      Ok(SendResponse::Success(headers, bytes.into()))
    }
  }
}

#[tokio::main]
#[allow(clippy::print_stdout)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let sys = RealSys;
  let cwd = sys.env_current_dir()?;
  let workspace_factory = Arc::new(WorkspaceFactory::new(sys.clone(), cwd, Default::default()));
  let resolver_factory = ResolverFactory::new(workspace_factory.clone(), Default::default());
  let resolver = resolver_factory.deno_resolver().await?;

  let file_fetcher = Arc::new(FileFetcher::new(
    NullBlobStore,
    sys.clone(),
    Arc::new(workspace_factory.http_cache()?.clone()),
    RolldownHttpClient { client: reqwest::Client::new() },
    Arc::new(NullMemoryFiles),
    FileFetcherOptions {
      allow_remote: true,
      auth_tokens: AuthTokens::new_from_sys(&sys),
      cache_setting: CacheSetting::Use,
    },
  ));
  let plugin = HttpImportPlugin { file_fetcher, resolver: resolver.clone() };

  let mut bundler = Bundler::with_plugins(
    BundlerOptions {
      input: Some(vec![InputItem {
        name: Some("text".to_string()),
        import: "jsr:@std/text@1".to_string(),
      }]),
      entry_filenames: Some(ChunkFilenamesOutputOption::String("[name].bundle.js".to_string())),
      cwd: Some(workspace::crate_dir("rolldown").join("./examples").normalize()),
      sourcemap: Some(SourceMapType::File),
      ..Default::default()
    },
    vec![Arc::new(plugin)],
  );
  bundler.options().input.iter().for_each(|input| println!("Bundle {}", input.import));

  let result = match bundler.write().await {
    Ok(result) => result,
    Err(err) => {
      panic!("{:?}", err);
    }
  };

  for asset in result.assets {
    eprintln!("Emit {:?}", asset.filename());
    println!("{}", String::from_utf8_lossy(asset.content_as_bytes()));
  }

  for err in result.warnings {
    eprintln!("{}", err);
  }

  Ok(())
}
