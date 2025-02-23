#![allow(clippy::disallowed_names)] // salsa bug (i8)

use std::sync::Arc;

use clap::Args;
use dashmap::DashMap;
use stc_ts_env::StableEnv;
use stc_ts_utils::StcComments;
use swc_common::{FileName, Globals, SourceMap, GLOBALS};
use tokio::task::{spawn_blocking, JoinHandle};
use tower_lsp::{
    async_trait,
    jsonrpc::{self},
    lsp_types::*,
    Client, LanguageServer, LspService, Server,
};
use tracing::{error, info};

use crate::ir::SourceFile;

pub mod config;
pub mod ir;
pub mod module_loader;
pub mod parser;
pub mod type_checker;

#[derive(Debug, Args)]
pub struct LspCommand {}

impl LspCommand {
    pub async fn run(self) -> anyhow::Result<()> {
        info!("Starting server");

        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();

        let (service, socket) = LspService::new(|client| {
            let cm = Arc::default();
            let globals = Arc::default();

            let stable_env = GLOBALS.set(&globals, StableEnv::new);
            let shared = Arc::new(Shared {
                client,
                stable_env,
                cm,
                globals,
                comments: Default::default(),
            });

            StcLangServer {
                project: Project::new(shared),
            }
        });
        Server::new(stdin, stdout, socket).serve(service).await;

        Ok(())
    }
}

pub struct StcLangServer {
    /// TODO: Per-tsconfig project
    project: Project,
}

pub struct Shared {
    #[allow(unused)]
    client: Client,
    cm: Arc<SourceMap>,
    globals: Arc<Globals>,
    stable_env: StableEnv,
    comments: StcComments,
}

/// One directory with `tsconfig.json`.
struct Project {
    #[allow(unused)]
    handle: JoinHandle<()>,

    sender: tokio::sync::Mutex<std::sync::mpsc::Sender<Request>>,
}

impl Project {
    fn new(shared: Arc<Shared>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Request>();

        let join_handle = spawn_blocking(move || {
            let mut db = Database {
                storage: Default::default(),
                shared,
                files: Default::default(),
            };

            let files = db.files.clone();

            while let Ok(req) = rx.recv() {
                match req {
                    Request::SetFileContent { filename, content } => match files.entry(filename.clone()) {
                        dashmap::mapref::entry::Entry::Occupied(mut e) => {
                            e.get_mut().set_content(&mut db).to(content);
                        }
                        dashmap::mapref::entry::Entry::Vacant(e) => {
                            e.insert(SourceFile::new(&db, filename, content));
                        }
                    },

                    Request::ValidateFile { filename } => {
                        let file = db.read_file(&filename);

                        let input = crate::type_checker::prepare_input(&db, file);
                        let _module_type = crate::type_checker::check_type(&db, input);

                        let diagnostics = crate::type_checker::check_type::accumulated::<crate::type_checker::Diagnostics>(&db, input);

                        dbg!(&diagnostics);
                    }
                }
            }
        });

        Self {
            handle: join_handle,
            sender: tx.into(),
        }
    }
}

enum Request {
    SetFileContent { filename: Arc<FileName>, content: String },
    ValidateFile { filename: Arc<FileName> },
}

#[async_trait]
impl LanguageServer for StcLangServer {
    async fn initialize(&self, _params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
                    open_close: Some(true),
                    change: None,
                    ..Default::default()
                })),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),

                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "stc-ts-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.project
            .sender
            .lock()
            .await
            .send(Request::ValidateFile {
                filename: to_filename(params.text_document.uri),
            })
            .expect("failed to send request");
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.project
            .sender
            .lock()
            .await
            .send(Request::SetFileContent {
                filename: to_filename(params.text_document.uri),
                content: params.content_changes[0].text.clone(),
            })
            .expect("failed to send request");
    }
}

#[salsa::jar(db = Db)]
pub struct Jar(
    crate::config::ParsedTsConfig,
    crate::config::tsconfig_for,
    crate::config::read_tsconfig_file_for,
    crate::config::parse_ts_config,
    crate::ir::SourceFile,
    crate::parser::ParserInput,
    crate::parser::ParsedFile,
    crate::parser::parse_ast,
    crate::module_loader::ProjectEnv,
    crate::module_loader::get_module_loader,
    crate::type_checker::TypeCheckInput,
    crate::type_checker::ModuleTypeData,
    crate::type_checker::Diagnostics,
    crate::type_checker::check_type,
    crate::type_checker::prepare_input,
);

pub trait Db: salsa::DbWithJar<Jar> {
    fn read_file(&self, path: &Arc<FileName>) -> SourceFile;

    fn shared(&self) -> &Arc<Shared>;
}

#[salsa::db(crate::Jar)]
pub(crate) struct Database {
    storage: salsa::Storage<Self>,

    shared: Arc<Shared>,

    files: Arc<DashMap<Arc<FileName>, SourceFile>>,
}

impl Db for Database {
    fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    fn read_file(&self, path: &Arc<FileName>) -> SourceFile {
        match self.files.get(path) {
            Some(file) => *file,
            None => {
                error!("File not found: {:?}", path);
                // TODO: Error
                SourceFile::new(self, path.clone(), "".to_string())
            }
        }
    }
}

impl salsa::Database for Database {}

fn to_filename(uri: Url) -> Arc<FileName> {
    if let Ok(v) = uri.to_file_path() {
        return Arc::new(FileName::Real(v));
    }
    Arc::new(FileName::Custom(uri.to_string()))
}
