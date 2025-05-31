use std::{path::PathBuf, sync::Arc};

use deno_cache_dir::file_fetcher::{CacheSetting, NullBlobStore};
use deno_graph::{MediaType, Module, ModuleGraph};
use deno_npm_installer::{
  NpmInstallerFactory, NpmInstallerFactoryOptions, lifecycle_scripts::NullLifecycleScriptsExecutor,
};
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
use sys_traits::{EnvCurrentDir, impls::RealSys};
use url::Url;

use self::http_client::RolldownHttpClient;

mod http_client;
mod module_analyzer;

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

#[tokio::main(flavor = "current_thread")]
#[allow(clippy::print_stdout)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let sys = RealSys;
  let cwd = sys.env_current_dir()?;
  let entrypoint = std::env::args().collect::<Vec<_>>().remove(1);
  let entrypoint = if entrypoint.starts_with("jsr:")
    || entrypoint.starts_with("https:")
    || entrypoint.starts_with("file:")
  {
    Url::parse(&entrypoint).unwrap()
  } else {
    deno_path_util::url_from_file_path(&cwd.join(entrypoint)).unwrap()
  };
  let workspace_factory = Arc::new(WorkspaceFactory::new(sys.clone(), cwd, Default::default()));
  let cwd = workspace_factory.initial_cwd();
  let resolver_factory =
    Arc::new(ResolverFactory::new(workspace_factory.clone(), Default::default()));
  let rolldown_client = RolldownHttpClient::default();
  let npm_installer_factory = NpmInstallerFactory::new(
    resolver_factory.clone(),
    Arc::new(rolldown_client.clone()),
    Arc::new(NullLifecycleScriptsExecutor),
    deno_npm_installer::LogReporter,
    NpmInstallerFactoryOptions {
      cache_setting: deno_npm_cache::NpmCacheSetting::Use,
      caching_strategy: deno_npm_installer::graph::NpmCachingStrategy::Eager,
      lifecycle_scripts_config: deno_npm_installer::LifecycleScriptsConfig {
        allowed: deno_npm_installer::PackagesAllowedScripts::None,
        initial_cwd: cwd.clone(),
        root_dir: workspace_factory.workspace_directory()?.workspace.root_dir_path(),
        explicit_install: false,
      },
      resolve_npm_resolution_snapshot: Box::new(|| Ok(None)),
    },
  );
  let npm_package_info_provider = npm_installer_factory.lockfile_npm_package_info_provider()?;
  let lockfile = workspace_factory.maybe_lockfile(npm_package_info_provider).await?;
  let resolver = resolver_factory.deno_resolver().await?;
  let cjs_tracker = resolver_factory.cjs_tracker()?;
  let jsx_config =
    ScopedJsxImportSourceConfig::from_workspace_dir(workspace_factory.workspace_directory()?)?;

  let file_fetcher = Arc::new(PermissionedFileFetcher::new(
    NullBlobStore,
    Arc::new(workspace_factory.http_cache()?.clone()),
    rolldown_client,
    sys.clone(),
    PermissionedFileFetcherOptions { allow_remote: true, cache_setting: CacheSetting::Use },
  ));
  let roots = Vec::from([entrypoint.clone()]);
  let graph_resolver = resolver.as_graph_resolver(cjs_tracker, &jsx_config);
  let loader = DenoGraphLoader::new(
    file_fetcher.clone(),
    workspace_factory.global_http_cache()?.clone(),
    resolver_factory.in_npm_package_checker()?.clone(),
    workspace_factory.sys().clone(),
    DenoGraphLoaderOptions { file_header_overrides: Default::default(), permissions: None },
  );

  let mut locker = lockfile.as_ref().map(|l| l.as_deno_graph_locker());
  let mut graph = deno_graph::ModuleGraph::new(deno_graph::GraphKind::CodeOnly);
  let npm_resolver = npm_installer_factory.npm_deno_graph_resolver().await?;
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
        locker: locker.as_mut().map(|l| l as _),
        file_system: &sys,
        jsr_url_provider: Default::default(),
        passthrough_jsr_specifiers: false,
        module_analyzer: &module_analyzer::OxcModuleAnalyzer,
        npm_resolver: Some(npm_resolver.as_ref()),
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
        name: Some(resolve_display_name(&entrypoint)),
        import: entrypoint.to_string(),
      }]),
      entry_filenames: Some(ChunkFilenamesOutputOption::String("[name].bundle.js".to_string())),
      cwd: Some(cwd.clone()),
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
    eprintln!("Writing: {}", asset.filename());
    std::fs::write(cwd.join(asset.filename()), asset.content_as_bytes()).unwrap();
  }

  for err in result.warnings {
    eprintln!("{}", err);
  }

  Ok(())
}

fn resolve_display_name(url: &Url) -> String {
  if let Ok(reference) = deno_semver::jsr::JsrPackageReqReference::from_specifier(url) {
    reference.req().name.split("/").skip(1).next().unwrap().to_string()
  } else if let Ok(reference) = deno_semver::npm::NpmPackageReqReference::from_specifier(url) {
    reference.req().name.to_string()
  } else if url.scheme() == "file" {
    // todo: improve lol
    PathBuf::from(url.as_str().split('/').last().unwrap().to_string())
      .file_stem()
      .unwrap()
      .to_string_lossy()
      .to_string()
  } else {
    "remote".to_string()
  }
}
