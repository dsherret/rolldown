use std::sync::Arc;

use deno_cache_dir::file_fetcher::{
  CacheSetting, HeaderMap, NullBlobStore, SendError, SendResponse,
};
use deno_graph::{DefaultModuleAnalyzer, MediaType, Module, ModuleGraph};
use deno_resolver::{
  factory::{ResolverFactory, WorkspaceFactory},
  file_fetcher::{
    DenoGraphLoader, DenoGraphLoaderOptions, PermissionedFileFetcher,
    PermissionedFileFetcherOptions,
  },
  graph::DefaultDenoResolverRc,
  workspace::ScopedJsxImportSourceConfig,
};
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
  file_fetcher: Arc<PermissionedFileFetcher<NullBlobStore, RealSys, RolldownHttpClient>>,
  graph: ModuleGraph,
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
      deno_graph::Position::zeroed(),
      match args.kind {
        ImportKind::Require => node_resolver::ResolutionMode::Require,
        _ => node_resolver::ResolutionMode::Import,
      },
      node_resolver::NodeResolutionKind::Execution,
    )?;
    let resolved = self.graph.resolve(&resolved);
    Ok(Some(HookResolveIdOutput { id: resolved.to_string().into(), ..Default::default() }))
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

    match self.graph.get(&url) {
      Some(Module::Js(js)) => Ok(Some(rolldown_plugin::HookLoadOutput {
        code: js.source.to_string(),
        module_type: Some(media_to_module_type(js.media_type)),
        ..Default::default()
      })),
      Some(Module::Json(json)) => Ok(Some(rolldown_plugin::HookLoadOutput {
        code: json.source.to_string(),
        module_type: Some(media_to_module_type(json.media_type)),
        ..Default::default()
      })),
      Some(Module::Wasm(_wasm)) => {
        panic!("Not supported.")
      }
      Some(Module::Node(_) | Module::Npm(_) | Module::External(_)) | None => {
        tokio::task::spawn_blocking(|| {
          // super inefficient...
          let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
          rt.block_on(async move {
            let file = file_fetcher.fetch_bypass_permissions(&url).await?;
            Ok(Some(rolldown_plugin::HookLoadOutput {
              code: String::from_utf8_lossy(&file.source).to_string(),
              module_type: Some(media_to_module_type(MediaType::from_specifier_and_headers(
                &url,
                file.maybe_headers.as_ref(),
              ))),
              ..Default::default()
            }))
          })
        })
        .await
        .unwrap()
      }
    }
  }
}

fn media_to_module_type(media_type: MediaType) -> ModuleType {
  match media_type {
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

#[tokio::main(flavor = "current_thread")]
#[allow(clippy::print_stdout)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let sys = RealSys;
  let cwd = sys.env_current_dir()?;
  let workspace_factory = Arc::new(WorkspaceFactory::new(sys.clone(), cwd, Default::default()));
  let resolver_factory = ResolverFactory::new(workspace_factory.clone(), Default::default());
  let resolver = resolver_factory.deno_resolver().await?;
  let cjs_tracker = resolver_factory.cjs_tracker()?;
  let jsx_config =
    ScopedJsxImportSourceConfig::from_workspace_dir(workspace_factory.workspace_directory()?)?;

  let file_fetcher = Arc::new(PermissionedFileFetcher::new(
    NullBlobStore,
    Arc::new(workspace_factory.http_cache()?.clone()),
    RolldownHttpClient { client: reqwest::Client::new() },
    sys.clone(),
    PermissionedFileFetcherOptions { allow_remote: true, cache_setting: CacheSetting::Use },
  ));
  let entrypoint = "jsr:@std/text@1";
  let roots = Vec::from([Url::parse(entrypoint).unwrap()]);
  let graph_resolver = resolver.as_graph_resolver(cjs_tracker, &jsx_config);
  let module_analyzer = DefaultModuleAnalyzer::default();
  let loader = DenoGraphLoader::new(
    file_fetcher.clone(),
    workspace_factory.global_http_cache()?.clone(),
    resolver_factory.in_npm_package_checker()?.clone(),
    workspace_factory.sys().clone(),
    DenoGraphLoaderOptions { file_header_overrides: Default::default(), permissions: None },
  );
  let mut graph = deno_graph::ModuleGraph::new(deno_graph::GraphKind::CodeOnly);
  graph
    .build(
      roots,
      Vec::new(),
      &loader,
      deno_graph::BuildOptions {
        is_dynamic: false,
        skip_dynamic_deps: false,
        module_info_cacher: Default::default(),
        executor: Default::default(),
        locker: None,
        file_system: &sys,
        jsr_url_provider: Default::default(),
        passthrough_jsr_specifiers: false,
        module_analyzer: &module_analyzer,
        npm_resolver: None,
        reporter: None,
        resolver: Some(&graph_resolver),
      },
    )
    .await;
  graph.valid().unwrap();

  let plugin = HttpImportPlugin { file_fetcher, resolver: resolver.clone(), graph };
  let mut bundler = Bundler::with_plugins(
    BundlerOptions {
      input: Some(vec![InputItem {
        name: Some("text".to_string()),
        import: entrypoint.to_string(),
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
